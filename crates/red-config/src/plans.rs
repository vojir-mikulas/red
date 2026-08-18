//! Saved transfer plans: named [`TransferPlan`]s persisted as JSON files.
//!
//! The sibling of [`crate::queries`], and for the same reason: a transfer worth
//! doing twice is worth naming. "Refresh the dev database from a filtered slice
//! of production, these three tables structure-only" is a decision, and RED had
//! nowhere to keep it.
//!
//! One file per plan under `<config>/red/plans/*.json`, beside `queries/`. JSON
//! rather than the `.sql` files' comment-header trick because a plan is a
//! structured value, not text with metadata bolted on - and because the CLI
//! (`red transfer --plan <file>`) reads exactly the same file the GUI wrote, so
//! the two paths finally share one artefact.
//!
//! Nothing is read at startup; callers invoke [`load`] on demand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use red_core::transfer::TransferPlan;

/// One saved plan: its display name and the plan itself.
#[derive(Clone, Debug)]
pub struct SavedPlan {
    /// The file's `name` field if present, else the un-slugged filename stem.
    pub name: String,
    pub plan: TransferPlan,
    /// The backing `.json` file, for a rename or a delete.
    pub path: PathBuf,
}

/// The on-disk shape. A wrapper rather than a bare [`TransferPlan`] so the file
/// can carry a display name that survives a rename of the file itself.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlanFile {
    name: String,
    plan: TransferPlan,
}

/// `<config>/red/plans`, the saved-plans directory.
fn plans_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("red").join("plans"))
}

/// Read every `*.json` in the plans dir, skipping (with a warning) any that
/// won't parse, so one bad file never blocks the others. Sorted by name
/// (case-insensitive). A missing dir is an empty list, never an error.
pub fn load() -> Vec<SavedPlan> {
    let Some(dir) = plans_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_file(&path) {
            Ok(plan) => out.push(plan),
            Err(e) => tracing::warn!("ignoring saved plan {}: {e}", path.display()),
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Read one plan file by path: what `red transfer --plan <file>` calls, so a
/// plan can live anywhere (a repo, a ticket attachment), not only in the config
/// directory.
pub fn read_file(path: &Path) -> Result<SavedPlan> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: PlanFile =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let name = if file.name.trim().is_empty() {
        crate::queries::slug(
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("transfer"),
        )
    } else {
        file.name
    };
    Ok(SavedPlan {
        name,
        plan: file.plan,
        path: path.to_path_buf(),
    })
}

/// Save `plan` under `name`, returning the file written. Atomic (temp file +
/// rename) so a crash can't leave a partial plan, and owner-only on Unix: a plan
/// can embed a `WHERE` clause with literal values, the same content class as the
/// query history and the saved queries.
pub fn save(name: &str, plan: &TransferPlan) -> Result<PathBuf> {
    use std::io::Write;

    let dir = plans_dir().context("no config directory for saved plans")?;
    std::fs::create_dir_all(&dir).context("creating the plans directory")?;
    let dest = dir.join(format!("{}.json", crate::queries::slug(name)));

    let file = PlanFile {
        name: name.trim().to_string(),
        plan: plan.clone(),
    };
    let contents = serde_json::to_string_pretty(&file).context("encoding the plan")?;

    let tmp = dest.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).context("creating the plan temp file")?;
    f.write_all(contents.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &dest).context("renaming the plan temp file")?;
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use red_core::transfer::{TransferItem, TransferOptions};

    fn sample() -> TransferPlan {
        let mut filtered = TransferItem::table("orders");
        filtered.content = red_core::transfer::ItemContent::Where("id > 3".into());
        TransferPlan {
            source_namespace: Some("prod".into()),
            target_namespace: Some("dev".into()),
            items: vec![TransferItem::table("users"), filtered],
            options: TransferOptions::default(),
        }
    }

    #[test]
    fn a_plan_round_trips_through_json() {
        // The saved artefact is what the CLI re-runs, so a lossy encode would
        // silently change what a re-run does.
        let file = PlanFile {
            name: "Nightly refresh".into(),
            plan: sample(),
        };
        let text = serde_json::to_string(&file).unwrap();
        let back: PlanFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.name, "Nightly refresh");
        assert_eq!(back.plan, sample());
    }

    #[test]
    fn a_named_plan_slugs_to_a_predictable_file() {
        assert_eq!(crate::queries::slug("Nightly refresh"), "nightly-refresh");
    }
}
