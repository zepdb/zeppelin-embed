// Reuse the shared C ABI fixture, linked only against the installed archive.
// Rust owns this executable's entry point; C owns the hand-written ABI layouts.
use std::ffi::CString;
unsafe extern "C" {
    fn ze_installed_fixture(argc: i32, argv: *const *const std::ffi::c_char) -> i32;
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<CString> = std::env::args()
        .map(CString::new)
        .collect::<Result<_, _>>()?;
    let pointers: Vec<_> = args.iter().map(|arg| arg.as_ptr()).collect();
    let status = unsafe { ze_installed_fixture(i32::try_from(pointers.len())?, pointers.as_ptr()) };
    if status != 0 {
        return Err(format!("installed fixture exited {status}").into());
    }
    Ok(())
}
