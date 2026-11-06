//! `join`: joins an online session from the console and plays as the next
//! free player with this PC's gamepads, without seeing the game (until the
//! client app receives the host's frames, this tests hosting).
//!
//! After joining, lines typed (or piped in) are commands: `press BUTTON
//! [FRAMES]` holds a Wii Remote button (`2`, `a`, `up`, ...; several joined
//! with `+`) for FRAMES frames (10 if left out), `wait FRAMES` waits before
//! the next command, and an empty line or `quit` leaves.

use crate::config::Config;
use crate::controls::{Bindings, Gamepads};
use crate::net::Session;
use anyhow::{bail, Result};
use rumble_runtime::input::button;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::mpsc::{channel, TryRecvError};
use std::time::{Duration, Instant};

fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("no input");
    }
    Ok(line.trim().to_string())
}

/// A command, by frames: hold `buttons` for `frames`.
struct Step {
    buttons: u32,
    frames: u32,
}

/// Reads a command line; `None` for leaving.
fn parse(line: &str) -> Option<Result<Step, String>> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let frames = |w: Option<&&str>, default| w.map_or(Ok(default), |w| w.parse().map_err(|_| format!("bad frame count {w:?}")));
    Some(match words.as_slice() {
        [] | ["quit"] => return None,
        ["press", names, rest @ ..] => names
            .split('+')
            .map(|n| button::by_name(n).ok_or(format!("unknown button {n:?}")))
            .collect::<Result<Vec<_>, _>>()
            .and_then(|bits| Ok(Step { buttons: bits.iter().fold(0, |a, b| a | b), frames: frames(rest.first(), 10)? })),
        ["wait", rest @ ..] => frames(rest.first(), 60).map(|frames| Step { buttons: 0, frames }),
        _ => Err(format!("unknown command {line:?} (press BUTTON [FRAMES], wait FRAMES, or an empty line to leave)")),
    })
}

pub fn run(cfg: &Config, addr: Option<String>) -> Result<()> {
    let name = match ask(&format!("Your name [{}]: ", cfg.online.name))? {
        n if n.is_empty() => cfg.online.name.clone(),
        n => n,
    };
    let addr = match addr {
        Some(a) => a,
        None => ask("Host (ip:port): ")?,
    };
    let session = Session::find(&addr)?;
    let players = if session.players == 1 { "1 player".to_string() } else { format!("{} players", session.players) };
    let answer = ask(&format!("{}'s multiplayer session found ({players}). Would you like to connect? [y/n] ", session.host))?;
    if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
        println!("Not connecting.");
        return Ok(());
    }
    let password = ask("Password: ")?;
    // It shows no picture: the smallest is asked for.
    let mut client = session.join(&password, &name, 720, false)?;
    println!(
        "Joined {}'s session as Player {}. Your gamepads play as Player {}. Commands: press BUTTON [FRAMES], wait FRAMES; an empty line leaves.",
        client.host, client.player, client.player
    );
    let (lines, commands) = channel();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        let done = std::io::stdin().read_line(&mut line).map_or(true, |n| n == 0);
        if lines.send(line).is_err() || done {
            break;
        }
    });

    let bindings = Bindings::new(&cfg.controls);
    let mut pads = Gamepads::new();
    let frame = Duration::from_micros(16_667);
    let mut next = Instant::now();
    let mut queue: VecDeque<Step> = VecDeque::new();
    let mut leaving = false;
    loop {
        // Commands queue up; the input reaching the end ends the queue.
        loop {
            match commands.try_recv() {
                Ok(line) => match parse(line.trim()) {
                    None => leaving = true,
                    Some(Ok(step)) => queue.push_back(step),
                    Some(Err(e)) => println!("{e}"),
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    leaving = true;
                    break;
                }
            }
        }
        let scripted = match queue.front_mut() {
            Some(step) => {
                step.frames = step.frames.saturating_sub(1);
                let bits = step.buttons;
                if step.frames == 0 {
                    queue.pop_front();
                }
                bits
            }
            None if leaving => break,
            None => 0,
        };
        let held = pads.pads(&bindings).iter().fold(scripted, |acc, p| acc | p.buttons);
        if let Err(e) = client.send_input(held, [0.0; 2]) {
            bail!("lost the connection: {e:#}");
        }
        next += frame;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    client.leave();
    println!("Left.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        let step = parse("press 2+a 5").unwrap().unwrap();
        assert_eq!((step.buttons, step.frames), (button::TWO | button::A, 5));
        assert_eq!(parse("press 1").unwrap().unwrap().frames, 10);
        assert_eq!(parse("wait 30").unwrap().unwrap().buttons, 0);
        assert!(parse("").is_none());
        assert!(parse("quit").is_none());
        assert!(parse("press 9").unwrap().is_err());
    }
}
