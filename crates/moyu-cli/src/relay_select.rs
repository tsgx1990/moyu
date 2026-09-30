//! `moyu init` relay-source selection: pick which relays a fresh account
//! persists, honoring (in priority) an explicit `--relay`, then a
//! `--relay-preset`, then — only on an interactive TTY — a 1/2 prompt, and
//! finally the public built-in default for non-interactive runs (scripts /
//! bots / CI), which must never block on a prompt.
//!
//! There is deliberately no hosted-relay preset: moyu does not operate
//! a relay. Users pick public relays or bring their own (see
//! `docs/self-host-relay.md`).

use moyu_core::transport;

/// Non-interactive relay preset chosen via `moyu init --relay-preset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RelayPreset {
    /// The public built-in relays (decentralized default). Skips the prompt.
    Public,
}

/// The choice a user makes at the interactive `init` prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitRelayPrompt {
    Public,
    Custom,
}

/// Where a selected relay set came from. Kept as an enum (not re-derived by
/// comparing relay lists) so callers can word their confirmation message by
/// *choice*, which stays correct if a preset's relay list ever changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelaySource {
    Public,
    Custom,
    CliFlag,
}

/// Decide the relays a fresh `moyu init` should persist, plus where the
/// choice came from. Precedence:
///  1. explicit `--relay` (non-empty `cli_relays`) — beats everything.
///  2. `--relay-preset public`.
///  3. interactive TTY → `prompt()` for a 1/2 choice (`Custom` → `custom_urls()`).
///  4. non-interactive → public built-in default (never blocks).
///
/// `prompt`/`custom_urls` are injected so this is a pure, unit-testable
/// decision with no real terminal I/O.
pub fn select_init_relays(
    cli_relays: &[String],
    preset: Option<RelayPreset>,
    is_tty: bool,
    mut prompt: impl FnMut() -> InitRelayPrompt,
    mut custom_urls: impl FnMut() -> Vec<String>,
) -> (Vec<String>, RelaySource) {
    if !cli_relays.is_empty() {
        return (cli_relays.to_vec(), RelaySource::CliFlag);
    }
    if let Some(p) = preset {
        return match p {
            RelayPreset::Public => (transport::default_relays(), RelaySource::Public),
        };
    }
    if is_tty {
        return match prompt() {
            InitRelayPrompt::Public => (transport::default_relays(), RelaySource::Public),
            InitRelayPrompt::Custom => (custom_urls(), RelaySource::Custom),
        };
    }
    (transport::default_relays(), RelaySource::Public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use moyu_core::transport;

    fn never_prompt() -> InitRelayPrompt {
        panic!("prompt must not be called")
    }
    fn no_custom() -> Vec<String> {
        panic!("custom must not be called")
    }

    #[test]
    fn explicit_relay_flag_wins_over_everything() {
        let got = select_init_relays(
            &["wss://x.example".to_string()],
            Some(RelayPreset::Public),
            true,
            never_prompt,
            no_custom,
        );
        assert_eq!(
            got,
            (vec!["wss://x.example".to_string()], RelaySource::CliFlag)
        );
    }

    #[test]
    fn preset_public_maps_to_default_relays_and_never_prompts() {
        let got = select_init_relays(
            &[],
            Some(RelayPreset::Public),
            true,
            never_prompt,
            no_custom,
        );
        assert_eq!(got, (transport::default_relays(), RelaySource::Public));
    }

    #[test]
    fn non_tty_without_flags_falls_back_to_public_default() {
        let got = select_init_relays(&[], None, false, never_prompt, no_custom);
        assert_eq!(got, (transport::default_relays(), RelaySource::Public));
    }

    #[test]
    fn tty_prompt_public_choice() {
        let got = select_init_relays(&[], None, true, || InitRelayPrompt::Public, no_custom);
        assert_eq!(got, (transport::default_relays(), RelaySource::Public));
    }

    #[test]
    fn tty_prompt_custom_returns_collected_urls() {
        let got = select_init_relays(
            &[],
            None,
            true,
            || InitRelayPrompt::Custom,
            || vec!["wss://self.host".to_string()],
        );
        assert_eq!(
            got,
            (vec!["wss://self.host".to_string()], RelaySource::Custom)
        );
    }
}
