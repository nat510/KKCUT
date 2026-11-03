//! HTTPS GET over WinHTTP, for the singer lists from Wikipedia.

use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows_sys::Win32::Networking::WinHttp::*;

pub struct Get {
    /// Largest accepted response, in bytes.
    pub limit: usize,
    pub timeout: Duration,
    /// False: fail instead of following a redirect to another address.
    pub redirects: bool,
}

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn handle(h: *mut c_void) -> Result<Handle, String> {
    if h.is_null() {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(Handle(h))
    }
}

fn check(ok: i32) -> Result<(), String> {
    if ok == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

fn split_url(url: &str) -> Result<(&str, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("Not an https address: {url}"))?;
    Ok(match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, "/"),
    })
}

pub fn get(url: &str, options: Get) -> Result<Vec<u8>, String> {
    let (host, path) = split_url(url)?;
    let wide = crate::wide;
    let start = Instant::now();
    unsafe {
        let session = handle(WinHttpOpen(
            // Wikimedia asks for an agent that says who is asking.
            wide(concat!(
                "KKCUT/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nat510/KKCUT)"
            ))
            .as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        check(WinHttpSetTimeouts(session.0, 3000, 5000, 10000, 15000))?;
        let connection = handle(WinHttpConnect(session.0, wide(host).as_ptr(), 443, 0))?;
        let request = handle(WinHttpOpenRequest(
            connection.0,
            wide("GET").as_ptr(),
            wide(path).as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        let mut disabled = WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_AUTHENTICATION;
        if !options.redirects {
            disabled |= WINHTTP_DISABLE_REDIRECTS;
        }
        check(WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            (&disabled as *const u32).cast(),
            4,
        ))?;
        check(WinHttpSendRequest(
            request.0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            0,
            0,
        ))?;
        check(WinHttpReceiveResponse(request.0, std::ptr::null_mut()))?;
        let mut status = 0u32;
        let mut size = 4u32;
        check(WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut size,
            std::ptr::null_mut(),
        ))?;
        if status != 200 {
            return Err(format!("{host} returned HTTP {status}"));
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            if start.elapsed() > options.timeout {
                return Err(format!("{host} timed out"));
            }
            let mut read = 0u32;
            check(WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ))?;
            if read == 0 {
                return Ok(body);
            }
            if body.len() + read as usize > options.limit {
                return Err(format!("{host}: response exceeds the size limit"));
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_addresses_and_refuses_plain_http() {
        assert_eq!(
            split_url("https://a.b/c/d?e=1").unwrap(),
            ("a.b", "/c/d?e=1")
        );
        assert_eq!(split_url("https://a.b").unwrap(), ("a.b", "/"));
        assert!(split_url("http://a.b/c").is_err());
    }
}
