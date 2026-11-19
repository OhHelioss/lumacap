//! Update check: asks api.github.com for the latest release tag. Nothing is downloaded or installed.
use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::*;


pub struct Release {
    pub tag: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
}

fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn http_get(host: &str, path: &str, max: usize) -> Option<Vec<u8>> {
    unsafe {
        let session = WinHttpOpen(windows::core::w!("lumacap"), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return None;
        }
        let mut body = Vec::new();
        let mut ok = false;
        let h = wz(host);
        let p = wz(path);
        let connect = WinHttpConnect(session, PCWSTR(h.as_ptr()), 443, 0);
        if !connect.is_null() {
            let req = WinHttpOpenRequest(connect, windows::core::w!("GET"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), std::ptr::null(), WINHTTP_FLAG_SECURE);
            if !req.is_null() {
                if WinHttpSendRequest(req, None, None, 0, 0, 0).is_ok() && WinHttpReceiveResponse(req, std::ptr::null_mut()).is_ok() {
                    let mut status = 0u32;
                    let mut len = 4u32;
                    let _ = WinHttpQueryHeaders(
                        req,
                        WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                        PCWSTR::null(),
                        Some(&mut status as *mut u32 as *mut c_void),
                        &mut len,
                        std::ptr::null_mut(),
                    );
                    if status == 200 {
                        let mut buf = vec![0u8; 64 * 1024];
                        ok = true;
                        loop {
                            let mut got = 0u32;
                            if WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got).is_err() {
                                ok = false;
                                break;
                            }
                            if got == 0 {
                                break;
                            }
                            body.extend_from_slice(&buf[..got as usize]);
                            if body.len() > max {
                                ok = false;
                                break;
                            }
                        }
                    }
                }
                let _ = WinHttpCloseHandle(req);
            }
            let _ = WinHttpCloseHandle(connect);
        }
        let _ = WinHttpCloseHandle(session);
        if ok { Some(body) } else { None }
    }
}

fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let k = format!("\"{key}\":\"");
    let i = text.find(&k)? + k.len();
    let j = text[i..].find('"')?;
    Some(&text[i..i + j])
}

pub fn fetch_latest() -> Option<Release> {
    let body = http_get("api.github.com", "/repos/TheBuildShelf/lumacap/releases/latest", 512 * 1024)?;
    let text = String::from_utf8_lossy(&body).to_string();
    let tag = field(&text, "tag_name")?.to_string();
    let mut url = None;
    let mut sha256 = None;
    if let Some(i) = text.find("\"name\":\"lumacap.exe\"") {
        let rest = &text[i..];
        let end = rest[10..].find("\"node_id\"").map(|e| e + 10).unwrap_or(rest.len().min(6000));
        let seg = &rest[..end.min(rest.len())];
        url = field(seg, "browser_download_url").map(|s| s.to_string());
        sha256 = field(seg, "digest").and_then(|d| d.strip_prefix("sha256:")).map(|s| s.to_ascii_lowercase());
        // browser_download_url appears after digest, possibly past the node_id of the next asset
        if url.is_none() {
            url = field(rest, "browser_download_url").map(|s| s.to_string());
        }
    }
    Some(Release { tag, url, sha256 })
}
