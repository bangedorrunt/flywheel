//! `flywheel ledger` subcommand types (split from `main.rs`, bd-qbi8).

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;

use crate::cmd::internal::{run_verify_with_timeout, tail_of};
use crate::config::Config;

#[derive(Subcommand)]
pub(crate) enum LedgerCmd {
    /// wrap `br gate list <id> --json` + legal_close; exit 0 only on a legal
    /// verdict bound to the current SHA (ADR-0019 Spec 1, SHA rule — no bare
    /// HEAD fallback)
    Check {
        bead_id: String,
        /// blast radius (ADR-0019): normal|high (default normal)
        #[arg(long, value_parser = ["normal", "high"], default_value = "normal")]
        blast: String,
        /// AC shape: checkable|judgment (default checkable)
        #[arg(long, value_parser = ["checkable", "judgment"], default_value = "checkable")]
        ac: String,
        /// repo dir for SHA derivation + br (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// repo to search for the citing commit when the work lives elsewhere
        /// (symmetric with `br close --repo`); default: the `--dir` repo
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// worker helper: write ONLY the receipt file
    /// `.flywheel/receipts/<bead-id>-<sha>.<kind>.txt` (refuses empty stdin).
    /// Workers never write verdicts — this writes evidence, not proof.
    Receipt {
        bead_id: String,
        /// verdict kind (kebab): command-verified | unit-test-verified |
        /// live-verified | reviewer-signed | worker-receipt |
        /// operator-directed | triage-verified
        #[arg(long)]
        kind: String,
        /// commit SHA (default: derived via git log --grep=<bead-id>)
        #[arg(long)]
        sha: Option<String>,
        /// ADR-0006 VERIFY piece: the bead close reason to check for a
        /// VERIFY tail + principles citations (reusable with the wave-swarm
        /// VERIFY stage). Advisory — findings print as lint warnings.
        #[arg(long)]
        close_reason: Option<String>,
        /// repo dir (default: current directory)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// strict vacuous VERIFY gate: refuse close when cargo test filter matches 0 tests (also env FLYWHEEL_VERIFY_STRICT=1)
        #[arg(long)]
        strict: bool,
    },
}

/// ADR-0019 `flywheel ledger`: br gate verdict store + receipt helper.
/// `check` exits 0 only on a legal verdict for the current SHA (SHA rule,
/// no bare-HEAD fallback). `receipt` writes only the receipt file.
pub(crate) fn cmd_ledger(_cfg: &Config, cmd: &LedgerCmd) -> Result<()> {
    match cmd {
        LedgerCmd::Check {
            bead_id,
            blast,
            ac,
            dir,
            repo,
        } => {
            let cwd = dir
                .clone()
                .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let search = repo.as_deref().unwrap_or(&cwd);
            let sha = crate::verdict::derive_head_sha_in(&cwd, search, bead_id);
            let mut bead = crate::verdict::bead_from_br_show_for_close(&cwd, bead_id)?;
            bead.blast = match blast.as_str() {
                "high" => crate::verdict::Blast::High,
                _ => crate::verdict::Blast::Normal,
            };
            let ac = match ac.as_str() {
                "judgment" => crate::verdict::AcShape::Judgment,
                _ => crate::verdict::AcShape::Checkable,
            };
            if let Ok(sha) = &sha
                && crate::verdict::ledger_check(&cwd, &bead, ac, sha)?
            {
                println!("ledger check {bead_id}: LEGAL (sha {sha})");
                return Ok(());
            }
            if let Some((task, verdict)) = crate::verdict::ledger_proved(&cwd, &bead, ac)? {
                println!("ledger check {bead_id}: LEGAL (task {task} verdict {verdict})");
                return Ok(());
            }
            let sha = match sha {
                Ok(sha) => sha,
                Err(e) if repo.is_none() => bail!(
                    "{e}\n  pass --repo <dir> when the work repo lives elsewhere \
                     (symmetric with br close --repo)"
                ),
                Err(e) => return Err(e),
            };
            let why = crate::verdict::ledger_check_diagnostics(&cwd, &bead, &sha)?;
            let detail = if why.is_empty() {
                String::new()
            } else {
                format!("\n  {}", why.join("\n  "))
            };
            anyhow::bail!(
                "ledger check {bead_id}: no legal verdict bound to sha {sha} \
                 (legal-close table, ADR-0019 Spec 1){detail}"
            );
        }
        LedgerCmd::Receipt {
            bead_id,
            kind,
            sha,
            close_reason,
            dir,
            strict,
        } => {
            let cwd = dir
                .clone()
                .unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            let kind = crate::verdict::VerdictKind::from_gate_name(kind)
                .ok_or_else(|| anyhow!("unknown verdict kind: {kind}"))?;
            let sha = match sha {
                Some(s) => s.clone(),
                None => crate::verdict::derive_head_sha(&cwd, bead_id)?,
            };
            let mut content = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut content)?;
            let path = crate::verdict::write_receipt(&cwd, bead_id, &sha, kind, &content)?;
            println!("receipt {}", path.display());
            if let Some(reason) = close_reason {
                let c = crate::verdict::check_close_reason(reason);
                if !c.verify_tail_present {
                    println!("lint: close reason carries no VERIFY tail (what was actually run)");
                }
                for w in &c.lint_warnings {
                    println!("lint: {w}");
                }
                if c.lint_warnings.is_empty() && c.verify_tail_present {
                    println!(
                        "close-reason check: clean ({} citation(s), verify tail present)",
                        c.citations.len()
                    );
                }
                match crate::verdict::verify_close_reason(reason) {
                    Ok(()) => println!("wave-verify gate: PASS"),
                    Err(e) => println!("wave-verify gate: REJECT ({e})"),
                }
            }
            let effective_strict = *strict
                || std::env::var("FLYWHEEL_VERIFY_STRICT")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
            if let Ok(bead) = crate::verdict::bead_from_br_show(&cwd, bead_id) {
                let verify = bead.verify.trim().to_string();
                if crate::is_cargo_test_verify(&verify) {
                    let timeout = crate::verdict::bead_timebox_secs(&cwd, bead_id, 120);
                    let timeout = timeout.min(300);
                    match run_verify_with_timeout(&verify, timeout, &cwd) {
                        Ok((exit, output)) => {
                            let (passed, filtered) = crate::parse_cargo_test_counts(&output);
                            if exit != 0 {
                                eprintln!(
                                    "verify gate: VERIFY command failed (exit {exit}) — see receipt {}",
                                    path.display()
                                );
                                let _ = crate::append_to_receipt(
                                    &path,
                                    &format!("\nVERIFY failed (exit {exit}):\n{output}\n"),
                                );
                            } else if passed == Some(0) {
                                let warning = format!(
                                    "WARNING: vacuous VERIFY — filter `{verify}` matched 0 tests (evidence proves nothing)"
                                );
                                eprintln!("{warning}");
                                println!("lint: {warning}");
                                let _ = crate::append_to_receipt(
                                    &path,
                                    &format!(
                                        "\n{warning}\nfiltered out: {}\noutput tail:\n{}\n",
                                        filtered
                                            .map(|n| n.to_string())
                                            .unwrap_or_else(|| "unknown".into()),
                                        tail_of(&output)
                                    ),
                                );
                                if effective_strict {
                                    bail!(warning);
                                }
                            } else if let Some(n) = passed {
                                let msg = format!("VERIFY matched {n} tests, all passed");
                                println!("verify gate: {msg} (filter `{verify}`)");
                                let _ = crate::append_to_receipt(
                                    &path,
                                    &format!(
                                        "\n{msg}\nfilter: {verify}\nfiltered out: {}\n",
                                        filtered
                                            .map(|n| n.to_string())
                                            .unwrap_or_else(|| "unknown".into())
                                    ),
                                );
                            } else {
                                if output.trim().is_empty() {
                                    let warning = format!(
                                        "WARNING: vacuous VERIFY — command `{verify}` produced empty output (evidence proves nothing)"
                                    );
                                    eprintln!("{warning}");
                                    println!("lint: {warning}");
                                    let _ =
                                        crate::append_to_receipt(&path, &format!("\n{warning}\n"));
                                    if effective_strict {
                                        bail!(warning);
                                    }
                                }
                            }
                        }
                        Err(tail) => {
                            eprintln!(
                                "verify gate: VERIFY timed out after {timeout}s — see receipt {}",
                                path.display()
                            );
                            let _ = crate::append_to_receipt(
                                &path,
                                &format!("\nVERIFY timed out after {timeout}s:\n{tail}\n"),
                            );
                        }
                    }
                }
            }
            Ok(())
        }
    }
}
