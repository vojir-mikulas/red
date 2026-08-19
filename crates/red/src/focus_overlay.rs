//! Hold-to-reveal focus hints: hold the trigger modifier alone and every surface
//! that can take keyboard focus paints the single key that jumps to it.
//!
//! It replaces three per-seam focus jumps (⌥⌘1/2/3, SQL-only) with one gesture
//! that works everywhere, because it is driven by [`crate::focus`]'s target
//! registry rather than by a hard-coded pane vocabulary.
//!
//! **Badges render inside their surface, not in a global overlay.** The obvious
//! build is a full-screen layer that positions each badge at its target's
//! captured window rect — but that needs every surface to publish its bounds
//! during paint, which is shared mutable state, a frame of latency before the
//! badges settle, and a clipping bug waiting to happen when a surface is scrolled
//! half off. Rendering the badge as an absolutely-positioned child of the surface
//! it labels costs one chained call per site and none of that: normal layout puts
//! it in the right place, and it clips with its parent for free.
//!
//! One full-screen layer does exist, but only to take the keyboard: while hints
//! are up it holds focus so the hint keys reach it before a focused editor can
//! swallow them as text. The focus it displaced is restored if the user releases
//! without choosing, so an abandoned peek costs nothing.

use std::time::Duration;

use flint::ActiveTheme;
use gpui::{
    AsyncApp, Context, FocusHandle, InteractiveElement, IntoElement, Modifiers,
    ModifiersChangedEvent, ParentElement, Styled, WeakEntity, Window, div, px,
};

use crate::app::AppState;
use crate::focus::{FocusTargetId, hint_alphabet};
use crate::settings::FocusTrigger;

/// Live hint state: the frozen target order and the focus to hand back.
pub(crate) struct FocusHints {
    /// The target ids in the order they were assigned hints, captured when the
    /// overlay opened. Frozen deliberately: the registry is rebuilt every frame,
    /// and a badge that renumbered itself under the user's finger — because a
    /// query finished and a result grid appeared — would be worse than useless.
    order: Vec<FocusTargetId>,
    /// The alphabet in force when the hints opened, frozen with the order for
    /// the same reason: a settings reload mid-hold must not repaint a badge as
    /// one character while the key the user is about to press means another.
    alphabet: &'static [char],
    /// Focus at the moment the hints appeared, restored if they are dismissed
    /// without a choice.
    restore: Option<FocusHandle>,
}

impl FocusHints {
    /// The hint painted on `id`, or `None` if it had none when the hints opened.
    pub(crate) fn hint(&self, id: FocusTargetId) -> Option<char> {
        let at = self.order.iter().position(|&t| t == id)?;
        self.alphabet.get(at).copied()
    }

    /// The target a hint character names, or `None` when the character has no
    /// badge on screen — a key in the alphabet that ran past the target count.
    fn target_for(&self, key: char) -> Option<FocusTargetId> {
        let key = key.to_ascii_lowercase();
        let at = self.alphabet.iter().position(|&c| c == key)?;
        self.order.get(at).copied()
    }
}

impl AppState {
    /// The hint for `id` while hints are showing. Every surface calls this at its
    /// own render site and paints a badge when it comes back `Some`.
    pub(crate) fn focus_hint(&self, id: FocusTargetId) -> Option<char> {
        self.focus_hints.as_ref()?.hint(id)
    }

    /// The trigger modifier changed. Arms, holds, or dismisses.
    ///
    /// Reached through the root div's modifier listener, which gpui dispatches
    /// down the *focus* path — so this only fires reliably because
    /// `ensure_focus_anchored` guarantees the root is always on that path. Before
    /// that invariant, focus adrift would have silently disabled the whole
    /// gesture, in exactly the states where the user most wants a way out.
    pub(crate) fn on_focus_modifiers(
        &mut self,
        event: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let trigger = self.settings.keymap.focus_overlay;
        if trigger == FocusTrigger::Off {
            return;
        }
        if self.trigger_held_alone(trigger, event.modifiers) {
            // Already armed or showing: holding is not a new gesture.
            if self.focus_hints.is_none() && !self.focus_arm_pending {
                self.arm_focus_hints(cx);
            }
            return;
        }
        // The trigger came up, or a second modifier joined it and this is a
        // chord after all. Either way the gesture is over.
        self.cancel_focus_hints(window, cx);
    }

    /// Whether `mods` is the bare trigger *and* hints are allowed right now.
    fn trigger_held_alone(&self, trigger: FocusTrigger, mods: Modifiers) -> bool {
        // A modal is a question; nothing else acts until it is answered — the
        // same rule the root's global actions follow.
        self.globals_enabled() && trigger.held_alone(mods)
    }

    /// Start the delay. A generation guard lets a later gesture supersede this
    /// one without the earlier timer having to be cancelled.
    fn arm_focus_hints(&mut self, cx: &mut Context<Self>) {
        self.focus_arm_gen = self.focus_arm_gen.wrapping_add(1);
        self.focus_arm_pending = true;
        let generation = self.focus_arm_gen;
        let delay = Duration::from_millis(self.settings.keymap.focus_overlay_delay_ms);
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| this.show_focus_hints(generation, cx))
                .ok();
        })
        .detach();
    }

    /// The delay elapsed: freeze the hint order and put the hints on screen.
    fn show_focus_hints(&mut self, generation: u64, cx: &mut Context<Self>) {
        // Superseded, or the trigger was released while the timer ran.
        if generation != self.focus_arm_gen || !self.focus_arm_pending {
            return;
        }
        self.focus_arm_pending = false;
        let order: Vec<FocusTargetId> = self.focus_targets(cx).iter().map(|t| t.id).collect();
        if order.is_empty() {
            return;
        }
        self.focus_hints = Some(FocusHints {
            order,
            alphabet: hint_alphabet(self.settings.keymap.focus_overlay_hints),
            restore: None,
        });
        // The layer takes focus on the next render, which is where a `Window` is
        // in hand; that is also where the displaced focus gets recorded.
        self.focus_hints_take_focus = true;
        cx.notify();
    }

    /// Dismiss without choosing: hand focus back to whatever had it.
    pub(crate) fn cancel_focus_hints(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_arm_pending = false;
        let Some(hints) = self.focus_hints.take() else {
            return;
        };
        if let Some(handle) = hints.restore {
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    /// A hint key was pressed: jump to the target it names.
    ///
    /// A character in the alphabet that ran past the target count has no badge on
    /// screen, so it dismisses like any other stray key rather than jumping
    /// somewhere the user was given no reason to expect.
    pub(crate) fn on_focus_hint_key(
        &mut self,
        key: char,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hints) = &self.focus_hints else {
            return;
        };
        let target = hints.target_for(key);
        // Drop the hints before focusing, so the layer is out of the tree by the
        // time the target takes focus and cannot bounce it back.
        self.focus_arm_pending = false;
        let restore = self.focus_hints.take().and_then(|h| h.restore);
        match target {
            Some(id) => {
                self.focus_target_by_id(id, window, cx);
            }
            None => {
                if let Some(handle) = restore {
                    window.focus(&handle, cx);
                }
            }
        }
        cx.notify();
    }

    /// Give the hint layer focus, remembering what it displaced. Called from
    /// `render`, the first point after `show_focus_hints` that holds a `Window`.
    pub(crate) fn take_focus_for_hints(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_hints_take_focus {
            return;
        }
        self.focus_hints_take_focus = false;
        let previous = window.focused(cx);
        if let Some(hints) = &mut self.focus_hints {
            hints.restore = previous;
        }
        window.focus(&self.focus_hints_focus.clone(), cx);
    }

    /// The transparent key-taking layer, rendered above the shell while hints
    /// show. Paints nothing: the badges live inside the surfaces they label.
    pub(crate) fn render_focus_hint_layer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            // The context the hint-key bindings are scoped to. It exists only
            // while this layer is mounted and holding focus, so those bindings
            // out-rank the `RedRoot` shortcuts some hint keys collide with.
            .key_context("FocusHints")
            .track_focus(&self.focus_hints_focus)
            .on_action(
                cx.listener(|this, action: &crate::keymap::FocusHintKey, window, cx| {
                    this.on_focus_hint_key(action.0, window, cx);
                }),
            )
            // Anything that is not a hint is a decision not to jump. Runs only
            // for keystrokes no binding claimed, which after the bindings above
            // means every non-hint key.
            .on_key_down(cx.listener(|this, _: &gpui::KeyDownEvent, window, cx| {
                this.cancel_focus_hints(window, cx);
                cx.stop_propagation();
            }))
            // A click anywhere is a decision not to jump.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| this.cancel_focus_hints(window, cx)),
            )
    }
}

/// The badge a surface paints while hints show: the key that jumps to it,
/// pinned to the surface's top-left corner.
///
/// The caller's container must be `relative()` — every surface that hosts one
/// already is, since they all position their own chrome.
pub(crate) fn badge(hint: char, cx: &gpui::App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .absolute()
        .top(px(6.))
        .left(px(6.))
        .px(px(7.))
        .py(px(2.))
        .rounded(px(4.))
        .bg(theme.accent)
        .border_1()
        .border_color(theme.accent)
        .text_color(theme.on_accent)
        .text_size(theme.scale(12.))
        .child(hint.to_ascii_uppercase().to_string())
}
