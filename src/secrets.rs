//! Секреты — в Windows Credential Manager (CLAUDE.md §8), не в config.json.
//! Видны в «Диспетчер учётных данных → Учётные данные Windows» как `Verba/…`.

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Security::Credentials::{
    CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
};

pub const OPENROUTER_KEY: &str = "Verba/OpenRouter";
pub const PROXY_PASSWORD: &str = "Verba/Proxy";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Секрет по имени; None — не сохранён.
pub fn read(target: &str) -> Option<String> {
    let name = wide(target);
    let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
    unsafe {
        CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred).ok()?;
        let c = &*cred;
        let bytes = std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize).to_vec();
        CredFree(cred as *const _);
        String::from_utf8(bytes).ok().filter(|s| !s.is_empty())
    }
}

pub fn write(target: &str, user: &str, secret: &str) -> Result<(), String> {
    let mut name = wide(target);
    let mut user_w = wide(user);
    let mut blob = secret.as_bytes().to_vec();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(name.as_mut_ptr()),
        UserName: PWSTR(user_w.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        ..Default::default()
    };
    let r = unsafe { CredWriteW(&cred, 0) };
    // секрет не держим в памяти дольше нужного
    blob.iter_mut().for_each(|b| *b = 0);
    r.map_err(|e| format!("Credential Manager не сохранил секрет: {e}"))
}

pub fn delete(target: &str) {
    let name = wide(target);
    unsafe {
        let _ = CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None);
    }
}
