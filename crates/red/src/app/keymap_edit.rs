//! The Keymap settings tab's behaviour: the chord recorder and the per-action
//! rebind / reset flow. The UI lives in [`crate::settings_ui`]; this is the state
//! machine behind it.
//!
//! The editor's model is **per action** ("this action uses this key"); the file
//! is **per keystroke**. [`crate::keymap`] owns the translation: [`effective_slots`]
//! reads the file into the per-row model the tab displays, and [`diff_blocks`]
//! writes a minimal override set back. This module never holds a parsed copy: the
//! file is the source of truth, and every edit reads the current slots, applies one
//! change, and saves, so the tab and a hand-edit stay in sync.
//!
//! The recorder captures one chord with [`App::intercept_keystrokes`], which runs
//! *before* binding dispatch and whose `stop_propagation` suppresses the action,
//! so capturing a colliding chord (`cmd-w`, `cmd-k`) is safe; the recorder is the
//! only consumer for that one keypress. The interceptor subscription is held
//! exactly as long as capture is live and dropped the instant a chord lands.
//!
//! [`effective_slots`]: crate::keymap::effective_slots
//! [`diff_blocks`]: crate::keymap::diff_blocks

use super::*;
use crate::keymap_config::KeymapBlock;

impl AppState {
    /// The editor's per-row keystroke model, read fresh from `keymap.toml` so it
    /// always reflects the on-disk truth (a hand-edit included). Every entry maps
    /// 1:1 to [`crate::keymap::action_defs`].
    pub(crate) fn keymap_slots(&self) -> crate::keymap::Slots {
        let blocks = self
            .keymap_store
            .as_ref()
            .map(|s| s.load_report().blocks)
            .unwrap_or_default();
        crate::keymap::effective_slots(&blocks)
    }

    /// Enter capture mode for `row`: install the keystroke interceptor and show the
    /// row's "press a shortcut" affordance. Any prior capture is discarded.
    ///
    /// The interceptor stays alive for the *whole* interaction: recording the
    /// first chord, and then the pending-confirm state where it lets the user
    /// re-press for a different chord, Enter to confirm, or Esc to cancel. It is
    /// the sole keyboard owner throughout, so the settings panel's own Esc/Enter
    /// listeners (which fire even when the interceptor stops propagation) stand
    /// down while it's live (see the gate in `render_settings`). It is dropped the
    /// instant the interaction ends (confirm, cancel, tab switch, or panel close)
    /// so normal shortcuts resume; a leaked interceptor would eat every keystroke
    /// app-wide.
    pub(crate) fn begin_keymap_record(&mut self, row: usize, cx: &mut Context<Self>) {
        self.keymap_capture = None;
        self.keymap_recording = Some(row);

        let weak = cx.entity().downgrade();
        let sub = cx.intercept_keystrokes(move |ev, _window, cx| {
            let ks = &ev.keystroke;
            // A bare modifier press has no key yet; wait for the real chord.
            // (Pure modifier changes aren't even key-down events, so this is just
            // defensive.)
            if ks.key.is_empty() {
                return;
            }
            let bare_escape = ks.key == "escape" && !ks.modifiers.modified();
            let bare_enter = ks.key == "enter" && !ks.modifiers.modified();
            let chord = ks.unparse();
            // Suppress whatever the chord is currently bound to: the recorder is
            // the sole consumer of this keypress, so capturing `cmd-w` / `cmd-k`
            // can't fire the action it would otherwise trigger.
            cx.stop_propagation();
            weak.update(cx, |this, cx| {
                if bare_escape {
                    this.cancel_keymap_record(cx);
                } else if this.keymap_capture.is_some() && bare_enter {
                    // A second Enter, with a chord already captured, confirms it.
                    this.confirm_keymap_rebind(cx);
                } else {
                    // First chord, or a re-press to pick a different one.
                    this.capture_keymap_chord(chord, cx);
                }
            })
            .ok();
        });
        self.keymap_intercept = Some(sub);
        cx.notify();
    }

    /// A chord landed: flag any same-context conflict and stash the pending rebind
    /// for the row's Confirm / Cancel. The interceptor stays live (see
    /// [`Self::begin_keymap_record`]) so a re-press can pick a different chord.
    pub(crate) fn capture_keymap_chord(&mut self, chord: String, cx: &mut Context<Self>) {
        // The row is whichever started recording, or the one already pending (a
        // re-press while confirming).
        let Some(row) = self
            .keymap_recording
            .or(self.keymap_capture.as_ref().map(|c| c.row))
        else {
            return;
        };
        self.keymap_recording = None;
        let slots = self.keymap_slots();
        let conflict = crate::keymap::conflict_for(&slots, row, &chord);
        self.keymap_capture = Some(KeymapCapture {
            row,
            chord,
            conflict,
        });
        cx.notify();
    }

    /// Cancel an in-flight capture or a pending rebind. The single place the
    /// recorder is torn down; a leaked interceptor would eat every keystroke
    /// app-wide, so [`Self::close_settings`] and tab switches route through here.
    pub(crate) fn cancel_keymap_record(&mut self, cx: &mut Context<Self>) {
        self.keymap_recording = None;
        self.keymap_intercept = None;
        self.keymap_capture = None;
        cx.notify();
    }

    /// Commit the pending rebind: move the row onto the captured chord (freeing a
    /// conflicting row first, so the new binding wins cleanly rather than shadowing
    /// it), then persist + apply the minimal override set.
    pub(crate) fn confirm_keymap_rebind(&mut self, cx: &mut Context<Self>) {
        let Some(cap) = self.keymap_capture.take() else {
            return;
        };
        let mut slots = self.keymap_slots();
        if let Some(loser) = cap.conflict {
            slots[loser] = None;
        }
        slots[cap.row] = Some(cap.chord);
        let blocks = crate::keymap::diff_blocks(&slots);
        self.save_keymap(blocks, cx);
    }

    /// Restore one row to its default keystroke and persist.
    pub(crate) fn reset_keymap_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let mut slots = self.keymap_slots();
        slots[row] = Some(crate::keymap::action_defs()[row].keystroke.to_string());
        let blocks = crate::keymap::diff_blocks(&slots);
        self.save_keymap(blocks, cx);
    }

    /// Restore every binding to its default, i.e. an empty override file.
    pub(crate) fn reset_all_keymap(&mut self, cx: &mut Context<Self>) {
        self.cancel_keymap_record(cx);
        self.save_keymap(Vec::new(), cx);
    }

    /// Write the override blocks to `keymap.toml` and apply them live. Mirrors
    /// [`Self::save_settings`] + [`Self::reload_keymap`]: announce the bytes to the
    /// watcher (so this write doesn't echo back as a reload), atomic-save, then
    /// re-apply the whole keymap and surface any per-binding warning. Clears the
    /// pending capture regardless of the save outcome.
    pub(crate) fn save_keymap(&mut self, blocks: Vec<KeymapBlock>, cx: &mut Context<Self>) {
        // The interaction is over: drop the interceptor and clear the recorder so
        // normal shortcuts resume.
        self.keymap_recording = None;
        self.keymap_intercept = None;
        self.keymap_capture = None;

        let Some(store) = self.keymap_store.clone() else {
            // No config dir: nothing to persist, but still apply live this session.
            self.keymap_warnings = crate::keymap::apply(cx, &blocks);
            cx.notify();
            return;
        };

        if let Ok(text) = crate::keymap_config::KeymapStore::serialize(&blocks) {
            if let Some(watcher) = &self.keymap_watcher {
                watcher.note_self_write(&text);
            }
        }
        if let Err(e) = store.save(&blocks) {
            self.notify(
                ToastVariant::Error,
                format!("Couldn't save keymap: {e}"),
                cx,
            );
            return;
        }
        self.keymap_warnings = crate::keymap::apply(cx, &blocks);
        cx.notify();
    }
}
