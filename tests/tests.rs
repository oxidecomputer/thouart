use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};

#[test]
fn test_portable_pty() -> Result<(), Box<dyn std::error::Error>> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize::default())?;

    // test_raw.rs just echoes the characters we feed it.
    let cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_test_raw"));

    let mut child = pair.slave.spawn_command(cmd)?;
    let mut reader = pair.master.try_clone_reader()?;
    let mut writer = pair.master.take_writer()?;

    // print some printable character, read until we see it, repeat once.
    // clears out garbage that windows echoes that interferes with our test.
    for msg in [b"@", b"%"] {
        let mut buf = [0u8];
        writer.write_all(msg)?;
        writer.flush()?;
        while &buf != msg {
            reader.read_exact(&mut buf)?;
        }
    }

    // the `3` in here would count as a ctrl+C and kill the process if we were
    // cooked, which we shouldn't be if we're in raw mode.
    writer.write_all(&[3])?;
    writer.flush()?;

    // again for windows, definitively flush out the junk the above produces,
    // then make sure the process is still alive after the ctrl+C.
    let mut buf = [0u8];
    writer.write_all(b"$")?;
    writer.flush()?;
    while &buf != b"$" {
        reader.read_exact(&mut buf)?;
    }
    assert!(child.try_wait()?.is_none());

    child.kill()?;

    Ok(())
}
