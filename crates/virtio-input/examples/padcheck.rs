//! Does this host have a controller, and which player would it be?
//!
//! ```console
//! cargo run -p virtio-input --example padcheck
//! ```
//!
//! Every gamepad test in this workspace is deliberately synthetic — a machine
//! with a pad plugged into it and a machine without must produce identical
//! results, or the acceptance is measuring the developer's desk. The cost of
//! that is that nothing in the test suite can tell you whether the *host* half
//! works on your machine, and "the guest pad does not move" has two very
//! different causes.
//!
//! So: this runs the real backend for the real player count, for a couple of
//! seconds, and prints what it found. `no controller found` on both players
//! with nothing plugged in is the correct answer, not a failure.
//!
//! It also shows the roster placing controllers: plug two in and player 1 is
//! the lower-keyed one, player 2 the other, and they never swap.

use std::time::Duration;

use virtio_input::{open_sources, Poll, SourceChoice, MAX_PLAYERS};

/// How long to wait for each player's source to find something. A source
/// rescans once a second, so two seconds is two chances.
const ATTEMPTS: usize = 40;
const STEP: Duration = Duration::from_millis(50);

fn main() {
    let players = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, MAX_PLAYERS);

    let (mechanism, factories) =
        open_sources(SourceChoice::Auto, players).expect("auto never fails on any host");
    println!("mechanism = {mechanism}, players = {}", factories.len());

    for (player, factory) in factories.iter().enumerate() {
        let mut source = factory();
        let mut found = None;
        for _ in 0..ATTEMPTS {
            if let Poll::Connected { id, state } = source.poll(STEP) {
                found = Some((id, state));
                break;
            }
        }
        match found {
            Some((id, state)) => println!(
                "player {}: CONNECTED label={:?} slot={} buttons={:?} sticks={:?}/{:?}",
                player + 1,
                id.label,
                id.slot,
                state.buttons,
                state.left_stick,
                state.right_stick
            ),
            None => println!("player {}: no controller found", player + 1),
        }
    }
}
