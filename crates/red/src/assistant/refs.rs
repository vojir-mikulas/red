//! Things in the UI the user can point the agent at.
//!
//! The composer is a text box in an app full of the things a question is *about*,
//! and half of what a schema-grounded agent gets wrong is resolving which object
//! the user meant. Pointing removes the ambiguity: drag `public.orders` onto the
//! panel, or pick "Ask AI about this", and the reference is exact.
//!
//! **Resolved late.** A chip holds a handle, not a snapshot: a tab's SQL can
//! change between the drop and the send, and what the model should get is what is
//! there when the user hits Enter. That is also what keeps a chip cheap.

use flint::ActiveTheme as _;
use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};

use red_service::ContextRefSpec;

/// Something in the UI dragged (or picked) into the assistant panel as a
/// reference for the next turn.
///
/// GPUI wants a drag payload that renders — the preview under the cursor *is*
/// the chip it will become — so this implements [`Render`] as the same chip the
/// composer draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ContextRef {
    Table {
        schema: String,
        name: String,
    },
    Column {
        schema: String,
        table: String,
        name: String,
    },
    Schema {
        name: String,
    },
    /// A query tab: the index is the handle the tab strip already drags, and
    /// `title` is what the chip reads.
    ///
    /// The title is carried rather than looked up at paint time — a chip has to
    /// name something the moment it lands, and re-resolving a label every frame
    /// to draw it would be a lot of work for a string. The tab's *SQL* is still
    /// resolved at send, so an edit between the drop and the send is what the
    /// model sees.
    Tab {
        index: usize,
        title: String,
    },
    /// The rows selected in a tab's result grid, resolved to a small text table
    /// at send.
    Rows {
        index: usize,
    },
}

impl ContextRef {
    /// Whether two references point at the same thing, ignoring the parts that
    /// are only there to be displayed.
    ///
    /// Dedup keys on this rather than on equality: a tab renamed between two
    /// drops is still the same tab, and two chips for it would send its SQL
    /// twice.
    pub(crate) fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (ContextRef::Tab { index: a, .. }, ContextRef::Tab { index: b, .. }) => a == b,
            _ => self == other,
        }
    }

    /// The chip's label: short, and enough to tell two references apart.
    pub(crate) fn label(&self) -> String {
        match self {
            ContextRef::Table { schema, name } if schema.is_empty() => name.clone(),
            ContextRef::Table { schema, name } => format!("{schema}.{name}"),
            ContextRef::Column { table, name, .. } => format!("{table}.{name}"),
            ContextRef::Schema { name } => name.clone(),
            ContextRef::Tab { title, .. } => title.clone(),
            ContextRef::Rows { .. } => "selected rows".to_string(),
        }
    }

    /// The chip's icon, so a table, a column and a tab are told apart at a glance.
    pub(crate) fn icon(&self) -> &'static str {
        match self {
            ContextRef::Table { .. } => "table",
            ContextRef::Column { .. } => "col",
            ContextRef::Schema { .. } => "schema",
            ContextRef::Tab { .. } => "file-text",
            ContextRef::Rows { .. } => "columns",
        }
    }

    /// Whether this reference carries row *data*, which the tier ladder gates
    /// separately from structure — the same rule the on-screen result shape
    /// already follows.
    pub(crate) fn is_data(&self) -> bool {
        matches!(self, ContextRef::Rows { .. })
    }

    /// The arms that need no live UI state, mapped straight onto the wire form.
    /// The rest are resolved by the panel, which can see the tabs.
    pub(crate) fn static_spec(&self) -> Option<ContextRefSpec> {
        match self {
            ContextRef::Table { schema, name } => Some(ContextRefSpec::Table {
                schema: schema.clone(),
                name: name.clone(),
            }),
            ContextRef::Column {
                schema,
                table,
                name,
            } => Some(ContextRefSpec::Column {
                schema: schema.clone(),
                table: table.clone(),
                name: name.clone(),
            }),
            ContextRef::Schema { name } => Some(ContextRefSpec::Schema { name: name.clone() }),
            ContextRef::Tab { .. } | ContextRef::Rows { .. } => None,
        }
    }
}

impl Render for ContextRef {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_1p5()
            .py(px(2.))
            .rounded(px(4.))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(theme.accent)
            .text_size(theme.scale(10.5))
            .text_color(theme.text)
            .child(crate::icons::icon(
                self.icon(),
                theme.scale(11.),
                theme.text_muted,
            ))
            .child(self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A structure reference needs nothing but itself; anything that names a
    /// piece of live UI has to be resolved against it, and saying which is which
    /// in one place is what stops a half-resolved reference reaching the model.
    #[test]
    fn only_ui_bound_references_need_resolving() {
        assert!(
            ContextRef::Table {
                schema: "public".into(),
                name: "orders".into(),
            }
            .static_spec()
            .is_some()
        );
        assert!(
            ContextRef::Tab {
                index: 0,
                title: "query 1".into()
            }
            .static_spec()
            .is_none()
        );
        assert!(ContextRef::Rows { index: 0 }.static_spec().is_none());
    }

    /// A renamed tab is still the same tab: dedup keys on identity, not on the
    /// label, or a rename between two drops sends its SQL twice.
    #[test]
    fn a_renamed_tab_is_the_same_target() {
        let before = ContextRef::Tab {
            index: 2,
            title: "query 3".into(),
        };
        let after = ContextRef::Tab {
            index: 2,
            title: "revenue by month".into(),
        };
        assert_ne!(before, after);
        assert!(before.same_target(&after));
        assert!(!before.same_target(&ContextRef::Tab {
            index: 3,
            title: "query 3".into(),
        }));
        // Everything else keys on equality, which is already identity.
        assert!(ContextRef::Rows { index: 1 }.same_target(&ContextRef::Rows { index: 1 }));
        assert!(!ContextRef::Rows { index: 1 }.same_target(&ContextRef::Rows { index: 2 }));
    }

    /// The chip names the tab, because "tab" names nothing.
    #[test]
    fn a_tab_chip_reads_as_the_tab() {
        assert_eq!(
            ContextRef::Tab {
                index: 0,
                title: "revenue by month".into()
            }
            .label(),
            "revenue by month"
        );
    }

    /// Row data is gated by tier; structure is not.
    #[test]
    fn rows_are_the_only_data_reference() {
        assert!(ContextRef::Rows { index: 1 }.is_data());
        assert!(
            !ContextRef::Schema {
                name: "public".into()
            }
            .is_data()
        );
        assert!(
            !ContextRef::Tab {
                index: 1,
                title: "query 2".into()
            }
            .is_data()
        );
    }

    /// The label is what the user reads on the chip, so a qualified name stays
    /// qualified and an unqualified engine does not grow a stray dot.
    #[test]
    fn labels_read_the_way_the_object_is_named() {
        assert_eq!(
            ContextRef::Table {
                schema: "public".into(),
                name: "orders".into()
            }
            .label(),
            "public.orders"
        );
        assert_eq!(
            ContextRef::Table {
                schema: String::new(),
                name: "orders".into()
            }
            .label(),
            "orders"
        );
        assert_eq!(
            ContextRef::Column {
                schema: "public".into(),
                table: "orders".into(),
                name: "total".into()
            }
            .label(),
            "orders.total"
        );
    }
}
