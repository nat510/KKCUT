//! The window. Closing it hides it: KKCUT keeps skipping from the
//! notification area until Exit is chosen there.

slint::include_modules!();

use crate::config::{Config, HISTORY_CHOICES};
use crate::{autostart, kkbox, tray};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

struct State {
    config: Config,
    lists: Arc<kkbox::Lists>,
    /// The rows on the Skipped tab, for the X and O buttons.
    shown: Vec<kkbox::Skipped>,
    fetching: bool,
}

thread_local! {
    static WINDOW: RefCell<Option<AppWindow>> = const { RefCell::new(None) };
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    /// Refreshes the window while it is shown.
    static TIMER: slint::Timer = slint::Timer::default();
}
/// A read of what KKBOX plays is under way (it runs off the UI thread).
static READING: AtomicBool = AtomicBool::new(false);

fn state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(s.borrow_mut().as_mut().expect("state is set in run")))
}

fn window(f: impl FnOnce(&AppWindow)) {
    WINDOW.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            f(w)
        }
    })
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> slint::ModelRc<T> {
    slint::ModelRc::new(slint::VecModel::from(rows))
}

pub fn run(hidden: bool) {
    let config = Config::load();
    let lists = Arc::new(kkbox::load().unwrap_or_default());
    kkbox::configure(&config, lists.clone());
    kkbox::spawn();

    let w = AppWindow::new().expect("the window opens");
    w.set_version(env!("CARGO_PKG_VERSION").into());
    w.set_enabled(config.enabled);
    w.set_artists(config.artists);
    w.set_titles(config.titles);
    w.set_black_enabled(config.black_enabled);
    w.set_author_enabled(config.author_enabled);
    w.set_history_index(
        HISTORY_CHOICES
            .iter()
            .position(|&n| n == config.history)
            .unwrap_or(1) as i32,
    );
    w.set_autostart(autostart::enabled());
    tray::spawn(on_tray, config.enabled);
    let first_run = lists.fetched == 0;
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            config,
            lists,
            shown: Vec::new(),
            fetching: false,
        })
    });
    wire(&w);
    w.window().on_close_requested(|| {
        TIMER.with(|t| t.stop());
        slint::CloseRequestResponse::HideWindow
    });
    repaint_when_back(&w);
    WINDOW.with(|cell| *cell.borrow_mut() = Some(w));
    refresh_all();
    if !hidden {
        show();
    }
    // The first start fetches the China list by itself.
    if first_run {
        start_fetch();
    }
    if let Err(e) = slint::run_event_loop_until_quit() {
        crate::log(&format!("Event loop stopped: {e}"));
    }
    tray::remove();
}

/// Runs on the icon's thread; the work is done on the window's.
fn on_tray(command: tray::Command) {
    let _ = slint::invoke_from_event_loop(move || match command {
        tray::Command::Show => show(),
        tray::Command::Toggle => {
            let on = state(|s| !s.config.enabled);
            window(|w| w.set_enabled(on));
            change(|c| c.enabled = on);
        }
        tray::Command::Quit => {
            tray::remove();
            let _ = slint::quit_event_loop();
        }
    });
}

fn show() {
    window(|w| {
        if let Err(e) = w.show() {
            crate::log(&format!("Window not shown: {e}"));
            return;
        }
        use slint::winit_030::WinitWindowAccessor;
        w.window().with_winit_window(|ww| {
            ww.set_minimized(false);
            ww.focus_window();
        });
        w.window().request_redraw();
    });
    refresh_all();
    TIMER.with(|t| {
        t.start(slint::TimerMode::Repeated, Duration::from_secs(2), || {
            update_now();
            update_skipped();
            window(|w| w.window().request_redraw());
        })
    });
}

/// After the window sat idle (minimized, covered, the screen off or the PC
/// asleep), Windows may show what was drawn before; it is drawn again as
/// soon as it is back, and the data refreshed.
fn repaint_when_back(w: &AppWindow) {
    use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
    w.window().on_winit_window_event(|window, event| {
        match event {
            WindowEvent::Focused(true) | WindowEvent::Occluded(false) => {
                window.request_redraw();
                let _ = slint::invoke_from_event_loop(refresh_all);
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                window.request_redraw()
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn refresh_all() {
    update_lists();
    update_list_info();
    update_skipped();
    update_now();
}

/// Saves a settings change and hands it to the guard and the icon.
fn change(set: impl FnOnce(&mut Config)) {
    let (config, lists) = state(|s| {
        set(&mut s.config);
        (s.config.clone(), s.lists.clone())
    });
    if let Err(e) = config.save() {
        crate::log(&format!("Settings not saved: {e}"));
    }
    kkbox::configure(&config, lists);
    tray::set_enabled(config.enabled);
    update_lists();
    update_skipped();
    update_now();
}

fn wire(w: &AppWindow) {
    w.on_set_enabled(|on| change(|c| c.enabled = on));
    w.on_set_artists(|on| change(|c| c.artists = on));
    w.on_set_titles(|on| change(|c| c.titles = on));
    w.on_set_black_enabled(|on| change(|c| c.black_enabled = on));
    w.on_set_author_enabled(|on| change(|c| c.author_enabled = on));
    w.on_history_changed(|i| {
        if let Some(&n) = HISTORY_CHOICES.get(i as usize) {
            change(|c| c.history = n);
        }
    });
    w.on_skip_delete(|i| {
        if let Some(s) = state(|s| s.shown.get(i as usize).cloned()) {
            kkbox::delete_skipped(s.when, &s.song.title);
            update_skipped();
        }
    });
    w.on_skip_allow(|i| {
        if let Some(s) = state(|s| s.shown.get(i as usize).cloned()) {
            // The artist that matched; for a title, the whole artist line.
            let name = if s.name.is_empty() {
                s.song.artist
            } else {
                s.name
            };
            change(|c| {
                c.list_add(false, &name);
            });
        }
    });
    w.on_list_add(|black, name| {
        change(|c| {
            c.list_add(black, &name);
        })
    });
    w.on_list_remove(|black, name| change(|c| c.list_remove(black, &name)));
    w.on_set_autostart(|on| {
        let result = autostart::set(on);
        window(|w| {
            w.set_autostart(autostart::enabled());
            w.set_autostart_error(match result {
                Ok(()) => "".into(),
                Err(e) => format!("無法變更自動啟動：{e}").into(),
            });
        });
    });
    w.on_refresh_list(start_fetch);
    w.on_open_site(|| open("https://github.com/nat510/KKCUT"));
    w.on_open_folder(|| {
        if let Some(dir) = crate::config::data_dir() {
            open(&dir.to_string_lossy());
        }
    });
    w.on_tab_changed(|_| refresh_all());
}

fn open(target: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, target) = (crate::wide("open"), crate::wide(target));
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
}

/// "剛剛", "5 分鐘前", "3 小時前", "2 天前".
fn ago(when: u64) -> String {
    match kkbox::now().saturating_sub(when) {
        s if s < 60 => "剛剛".into(),
        s if s < 3600 => format!("{} 分鐘前", s / 60),
        s if s < 86400 => format!("{} 小時前", s / 3600),
        s => format!("{} 天前", s / 86400),
    }
}

fn update_lists() {
    let (black, white) = state(|s| (s.config.black.clone(), s.config.white.clone()));
    window(|w| {
        w.set_black_names(model(black.into_iter().map(Into::into).collect()));
        w.set_white_names(model(white.into_iter().map(Into::into).collect()));
    });
}

fn list_text(lists: &kkbox::Lists) -> (String, i32) {
    if lists.fetched == 0 {
        (
            "還沒有中國歌手名單。下載完成前，只會依簡體中文歌名與黑名單切歌。".into(),
            2,
        )
    } else {
        (
            format!(
                "共 {} 位中國歌手與樂團；另有 {} 位台港澳星馬歌手與樂團不會被切。取自 {} 個維基百科分類，{}更新。",
                lists.china.len(),
                lists.elsewhere.len(),
                lists.categories,
                ago(lists.fetched)
            ),
            0,
        )
    }
}

fn update_list_info() {
    let (fetching, lists) = state(|s| (s.fetching, s.lists.clone()));
    if fetching {
        return; // The fetch writes its own progress.
    }
    let (text, state) = list_text(&lists);
    window(|w| {
        w.set_list_info(text.into());
        w.set_list_state(state);
    });
}

fn set_list_info(text: String, state: i32) {
    window(|w| {
        w.set_list_info(text.into());
        w.set_list_state(state);
    });
}

fn start_fetch() {
    if state(|s| std::mem::replace(&mut s.fetching, true)) {
        return;
    }
    set_list_info("正在從維基百科下載中國歌手名單…".into(), 1);
    // The last fetch's count, for an idea of how far this one is.
    let expected = state(|s| s.lists.categories);
    std::thread::spawn(move || {
        let start = Instant::now();
        let progress = |n: usize| {
            let secs = start.elapsed().as_secs();
            let of = if expected > 0 {
                format!("（上次共 {expected} 個）")
            } else {
                String::new()
            };
            let text = format!(
                "正在從維基百科下載中國歌手名單：已讀取 {n} 個分類{of}，經過 {}:{:02}。維基百科要求一次只送一個請求，需要幾分鐘。",
                secs / 60,
                secs % 60
            );
            let _ = slint::invoke_from_event_loop(move || set_list_info(text, 1));
        };
        let result = kkbox::refresh(&progress);
        let _ = slint::invoke_from_event_loop(move || {
            state(|s| s.fetching = false);
            match result {
                Ok(lists) => {
                    let lists = Arc::new(lists);
                    let config = state(|s| {
                        s.lists = lists.clone();
                        s.config.clone()
                    });
                    kkbox::configure(&config, lists);
                    update_list_info();
                    update_now();
                }
                Err(e) => {
                    crate::log(&format!("China list not fetched: {e}"));
                    let lists = state(|s| s.lists.clone());
                    let (before, _) = list_text(&lists);
                    set_list_info(format!("更新失敗：{e}。{before}"), 2);
                }
            }
        });
    });
}

fn update_skipped() {
    let (keep, rules) = state(|s| (s.config.history, kkbox::Rules::of(&s.config)));
    let shown: Vec<kkbox::Skipped> = kkbox::skipped().into_iter().take(keep).collect();
    let rows = shown
        .iter()
        .map(|s| SkipRow {
            title: s.song.title.clone().into(),
            artist: s.song.artist.clone().into(),
            detail: format!("{} · {}", ago(s.when), s.reason).into(),
            allowed: rules.allowed(&s.song.artist),
        })
        .collect();
    state(|s| s.shown = shown);
    window(|w| w.set_skipped(model(rows)));
}

fn update_now() {
    if READING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        let now = kkbox::now_playing();
        READING.store(false, Ordering::Release);
        let now = now.map_err(|e| e.to_string());
        let _ = slint::invoke_from_event_loop(move || {
            let (state_text, song, playing) = match now {
                Err(e) => (format!("無法讀取 KKBOX：{e}"), None, false),
                Ok(None) => ("KKBOX 沒有開啟，或還沒播放任何歌曲".into(), None, false),
                Ok(Some((song, playing))) => (
                    if playing { "播放中" } else { "已暫停" }.into(),
                    Some(song),
                    playing,
                ),
            };
            let verdict = song
                .as_ref()
                .and_then(|song| {
                    state(|s| {
                        let reason =
                            kkbox::judge(song, &s.lists, &kkbox::Rules::of(&s.config))?;
                        Some(if s.config.enabled {
                            format!("會被切掉：{reason}")
                        } else {
                            format!("開啟「啟動」後會被切掉：{reason}")
                        })
                    })
                })
                .unwrap_or_default();
            let song = song.unwrap_or_default();
            window(|w| {
                w.set_now_state(state_text.into());
                w.set_now_playing(playing);
                w.set_now_title(song.title.into());
                w.set_now_artist(song.artist.into());
                w.set_now_album(song.album.into());
                w.set_now_verdict(verdict.into());
            });
        });
    });
}
