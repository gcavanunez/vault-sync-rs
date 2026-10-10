use crate::clock;
use crate::config::Settings;
use crate::git::Git;
use crate::model::{Failure, Kind, Outcome, Snapshot, State, failure};

/// One synchronization pass. Failures are recorded in the returned state;
/// only an interrupt is returned as an error, because it is not a result.
pub fn sync_once(
    git: &Git<'_>,
    settings: &Settings,
    previous: &State,
    network: bool,
) -> Result<State, Failure> {
    let mut state = State {
        last_attempt: Some(clock::now_iso()),
        ..previous.clone()
    };

    match pass(git, settings, &mut state, network) {
        Ok(next) => Ok(next),
        Err(error) if error.kind == Kind::Interrupted => Err(error),
        Err(error) => Ok(State {
            outcome: Outcome::of(error.kind),
            failures: if error.kind == Kind::Pending {
                0
            } else {
                state.failures + 1
            },
            message: error.message,
            ..state
        }),
    }
}

fn pass(
    git: &Git<'_>,
    settings: &Settings,
    state: &mut State,
    network: bool,
) -> Result<State, Failure> {
    if state.paused {
        return Ok(State {
            outcome: Outcome::Paused,
            message: "Automation is paused".into(),
            ..state.clone()
        });
    }

    git.ready()?;
    git.run(&["add", "-A", "--", "."])?;
    let staged = git.output(&["diff", "--cached", "--quiet", "--exit-code"])?;

    match staged.code {
        0 => {}
        1 => {
            let subject = format!(
                "vault backup: {} {}",
                settings.device_name,
                clock::local_stamp(clock::now_ms())
            );
            let files = git.run(&["diff", "--cached", "--name-status"])?.stdout;
            git.run(&["commit", "-m", &subject, "-m", files.trim()])?;
        }
        _ => return failure(Kind::Blocked, "Cannot inspect staged changes"),
    }

    if !network {
        return Ok(State {
            outcome: Outcome::Offline,
            message: "Saved local changes; waiting before the next remote attempt".into(),
            ..state.clone()
        });
    }

    git.fetch()?;
    state.last_contact = Some(clock::now_iso());
    git.integrate()?;
    let mut snapshot = git.head()?;

    if git.counts()?.ahead > 0 {
        let refspec = format!("{snapshot}:refs/heads/{}", settings.branch);
        let pushed = git.output(&["push", "--porcelain", &settings.remote, &refspec])?;

        if pushed.code != 0 {
            git.fetch()?;
            state.last_contact = Some(clock::now_iso());
            git.integrate()?;

            if git.counts()?.ahead > 0 {
                return failure(
                    Kind::Blocked,
                    format!(
                        "Push failed; local commits are preserved. {}",
                        pushed.stderr.trim()
                    ),
                );
            }

            snapshot = git.head()?;
        }

        state.last_contact = Some(clock::now_iso());
    }

    state.last_synced = Some(Snapshot {
        commit: snapshot.clone(),
        time: clock::now_iso(),
    });
    state.failures = 0;
    let short = &snapshot[..snapshot.len().min(12)];

    if git.head()? != snapshot || git.dirty()? {
        return Ok(State {
            outcome: Outcome::Pending,
            message: format!("Snapshot {short} synced; newer local changes remain"),
            ..state.clone()
        });
    }

    Ok(State {
        outcome: Outcome::Synced,
        last_clean: Some(clock::now_iso()),
        message: format!("Snapshot {short} synced; no pending local changes at completion"),
        ..state.clone()
    })
}
