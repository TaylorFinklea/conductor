use std::{
    io::{self, Error, ErrorKind, Write},
    time::Duration,
};

use crate::{
    event::{filter::CursorPositionFilter, poll_internal, read_internal, InternalEvent},
    terminal::{disable_raw_mode, enable_raw_mode, sys::is_raw_mode_enabled},
};

/// Returns the cursor position (column, row).
///
/// The top left cell is represented as `(0, 0)`.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
pub fn position() -> io::Result<(u16, u16)> {
    if is_raw_mode_enabled() {
        read_position_raw()
    } else {
        read_position()
    }
}

fn read_position() -> io::Result<(u16, u16)> {
    enable_raw_mode()?;
    let pos = read_position_raw();
    disable_raw_mode()?;
    pos
}

fn read_position_raw() -> io::Result<(u16, u16)> {
    // Use `ESC [ 6 n` to and retrieve the cursor position.
    let mut stdout = io::stdout();
    stdout.write_all(b"\x1B[6n")?;
    stdout.flush()?;

    wait_for_position_response(
        |timeout| poll_internal(timeout, &CursorPositionFilter),
        || read_internal(&CursorPositionFilter),
    )
}

fn wait_for_position_response<P, R>(mut poll: P, mut read: R) -> io::Result<(u16, u16)>
where
    P: FnMut(Option<Duration>) -> io::Result<bool>,
    R: FnMut() -> io::Result<InternalEvent>,
{
    loop {
        match poll(Some(Duration::from_millis(2000))) {
            Ok(true) => {
                if let Ok(InternalEvent::CursorPosition(x, y)) = read() {
                    return Ok((x, y));
                }
            }
            Ok(false) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "The cursor position could not be read within a normal duration",
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_response_is_returned() {
        let result =
            wait_for_position_response(|_| Ok(true), || Ok(InternalEvent::CursorPosition(3, 4)));
        assert_eq!(result.unwrap(), (3, 4));
    }

    #[test]
    fn poll_timeout_is_an_error() {
        let result = wait_for_position_response(
            |_| Ok(false),
            || panic!("read must not be called when poll reports no event"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn poll_error_is_propagated_not_retried() {
        let mut polls = 0;
        let result = wait_for_position_response(
            |_| {
                polls += 1;
                if polls == 1 {
                    Err(Error::new(ErrorKind::Other, "Input/output error (EIO)"))
                } else {
                    Ok(true)
                }
            },
            || Ok(InternalEvent::CursorPosition(1, 1)),
        );
        assert!(result.is_err());
        assert_eq!(polls, 1, "poll must not be retried after an error");
    }
}
