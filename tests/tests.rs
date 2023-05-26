use std::io::{Read, Write};
use portable_pty::{CommandBuilder, native_pty_system, PtySize};

#[test]
fn test_portable_pty() -> Result<(), Box<dyn std::error::Error>> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize::default())?;

    let cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_test_raw"));

    let mut child = pair.slave.spawn_command(cmd)?;
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    // test_raw.rs just echoes its input back
    for msg in [b"abc", b"def"] {
        let mut buf = [0u8; 3];
        writer.write_all(msg)?;
        writer.flush()?;
        reader.read_exact(&mut buf)?;
        assert_eq!(&buf, msg);
    }

    let mut buf = [0; 2];
    // the `3` in here would count as a ctrl+C and kill the process if we were
    // cooked, which we shouldn't be if we're in raw mode.
    writer.write_all(&[3])?;
    writer.flush()?;
    reader.read(&mut buf)?;
    assert!(child.try_wait()?.is_none());

    // escape sequence in test_raw.rs is [1, 2], so this should end it
    writer.write_all(&[1, 2])?;
    writer.flush()?;
    reader.read(&mut buf)?;
    child.wait()?;

    Ok(())
}

/*
#[cfg(target_family = "windows")]
mod tests {
    use std::ptr::null_mut;
    use libloading::{Library, Symbol};
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::winerror::S_OK;
    use winapi::um::namedpipeapi::CreatePipe;
    use winapi::um::winnt::{HANDLE, HRESULT};
    use winapi::um::wincon::COORD;

    #[test]
    fn test_conpty() -> Result<(), Box<dyn std::error::Error>> {
        unsafe {
            let dll = Library::new("kernel32")?;
            let create_pseudo_console: Symbol<
                unsafe extern "C" fn(COORD, HANDLE, HANDLE, DWORD, *mut HANDLE) -> HRESULT
            > = dll.get(b"CreatePseudoConsole")?;
            let close_pseudo_console: Symbol<
                unsafe extern "C" fn(HANDLE)
            > = dll.get(b"ClosePseudoConsole")?;

            let mut in_test = null_mut();
            let mut in_con = null_mut();
            let mut out_test = null_mut();
            let mut out_con = null_mut();
            let mut device = null_mut();

            CreatePipe(&mut in_con, &mut in_test, null_mut(), 0);
            CreatePipe(&mut out_test, &mut out_con, null_mut(), 0);

            let res = create_pseudo_console(COORD { X: 80, Y: 25 }, in_con, out_con, 0, &mut device);
            assert_eq!(res, S_OK);

            close_pseudo_console(device);
            Ok(())
        }
    }
}
*/