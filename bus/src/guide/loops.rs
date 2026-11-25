//! Loop recipes for sh and PowerShell 5.1 (section 8.3).

use super::{GuideRole, Shell};

pub fn recipe(role: GuideRole, shell: Shell) -> &'static str {
    match (role, shell) {
        (GuideRole::Worker, Shell::Sh) => WORKER_SH,
        (GuideRole::Worker, Shell::Ps) => WORKER_PS,
        (GuideRole::Lead, Shell::Sh) => LEAD_SH,
        (GuideRole::Lead, Shell::Ps) => LEAD_PS,
        (GuideRole::Coordinator, Shell::Sh) => COORD_SH,
        (GuideRole::Coordinator, Shell::Ps) => COORD_PS,
        (GuideRole::Brain, Shell::Sh) => BRAIN_SH,
        (GuideRole::Brain, Shell::Ps) => BRAIN_PS,
        (GuideRole::GuestLead, Shell::Sh) => GUEST_LEAD_SH,
        (GuideRole::GuestLead, Shell::Ps) => GUEST_LEAD_PS,
    }
}

pub fn render(role: GuideRole, shell: Option<Shell>) -> String {
    match shell {
        Some(s) => recipe(role, s).to_owned(),
        None => format!("{}\n{}", recipe(role, Shell::Sh), recipe(role, Shell::Ps)),
    }
}

const WORKER_SH: &str = r#"# sh. Ends when a task is leased (code 0), when .sommelier/stop exists, or after 8 hours.
end=$(( $(date +%s) + 28800 ))
while [ ! -e .sommelier/stop ] && [ "$(date +%s)" -lt "$end" ]; do
  sommelier next --wait 300 --json > .sommelier/next.json; rc=$?
  case $rc in
    0) exit 0 ;;
    4) sommelier sync ;;
    5) sleep "$(sommelier quota wait-secs)" ;;
    6) sleep 60 ;;
    *) sleep 30 ;;
  esac
done
exit 6
"#;

const WORKER_PS: &str = r#"# PowerShell 5.1. Same behaviour.
$end = (Get-Date).AddHours(8)
while (-not (Test-Path .sommelier/stop) -and (Get-Date) -lt $end) {
  sommelier next --wait 300 --json | Out-File -Encoding utf8 .sommelier/next.json
  $rc = $LASTEXITCODE
  if ($rc -eq 0) { exit 0 }
  elseif ($rc -eq 4) { sommelier sync }
  elseif ($rc -eq 5) { Start-Sleep -Seconds ([int](sommelier quota wait-secs)) }
  elseif ($rc -eq 6) { Start-Sleep -Seconds 60 }
  else { Start-Sleep -Seconds 30 }
}
exit 6
"#;

const LEAD_SH: &str = r#"# sh. Lead digest loop.
while [ ! -e .sommelier/stop ]; do
  sommelier digest --role lead --wait 600; rc=$?
  case $rc in
    0) exit 0 ;;
    4) sommelier sync ;;
    5) sleep "$(sommelier quota wait-secs)" ;;
    6) sleep 60 ;;
    *) sleep 30 ;;
  esac
done
"#;

const LEAD_PS: &str = r#"# PowerShell 5.1. Lead digest loop.
while (-not (Test-Path .sommelier/stop)) {
  sommelier digest --role lead --wait 600
  $rc = $LASTEXITCODE
  if ($rc -eq 0) { exit 0 }
  elseif ($rc -eq 4) { sommelier sync }
  elseif ($rc -eq 5) { Start-Sleep -Seconds ([int](sommelier quota wait-secs)) }
  elseif ($rc -eq 6) { Start-Sleep -Seconds 60 }
  else { Start-Sleep -Seconds 30 }
}
"#;

const COORD_SH: &str = r#"# sh. Coordinator open alerts loop.
while [ ! -e .sommelier/stop ]; do
  sommelier alerts --open --wait 1800; rc=$?
  case $rc in
    0) exit 0 ;;
    4) sommelier sync ;;
    5) sleep "$(sommelier quota wait-secs)" ;;
    6) sleep 60 ;;
    *) sleep 30 ;;
  esac
done
"#;

const COORD_PS: &str = r#"# PowerShell 5.1. Coordinator open alerts loop.
while (-not (Test-Path .sommelier/stop)) {
  sommelier alerts --open --wait 1800
  $rc = $LASTEXITCODE
  if ($rc -eq 0) { exit 0 }
  elseif ($rc -eq 4) { sommelier sync }
  elseif ($rc -eq 5) { Start-Sleep -Seconds ([int](sommelier quota wait-secs)) }
  elseif ($rc -eq 6) { Start-Sleep -Seconds 60 }
  else { Start-Sleep -Seconds 30 }
}
"#;

const BRAIN_SH: &str = r#"# sh. Brain backlog and consult loop.
while [ ! -e .sommelier/stop ]; do
  sommelier alerts --code BACKLOG_LOW,CONSULT --wait 3600; rc=$?
  case $rc in
    0) exit 0 ;;
    4) sommelier sync ;;
    5) sleep "$(sommelier quota wait-secs)" ;;
    6) sleep 60 ;;
    *) sleep 30 ;;
  esac
done
"#;

const BRAIN_PS: &str = r#"# PowerShell 5.1. Brain backlog and consult loop.
while (-not (Test-Path .sommelier/stop)) {
  sommelier alerts --code BACKLOG_LOW,CONSULT --wait 3600
  $rc = $LASTEXITCODE
  if ($rc -eq 0) { exit 0 }
  elseif ($rc -eq 4) { sommelier sync }
  elseif ($rc -eq 5) { Start-Sleep -Seconds ([int](sommelier quota wait-secs)) }
  elseif ($rc -eq 6) { Start-Sleep -Seconds 60 }
  else { Start-Sleep -Seconds 30 }
}
"#;

const GUEST_LEAD_SH: &str = r#"# sh. Guest lead own mode offers loop.
while [ ! -e .sommelier/stop ]; do
  sommelier offers --top 5 --wait 600; rc=$?
  case $rc in
    0) exit 0 ;;
    4) sommelier sync ;;
    5) sleep "$(sommelier quota wait-secs)" ;;
    6) sleep 60 ;;
    *) sleep 30 ;;
  esac
done
"#;

const GUEST_LEAD_PS: &str = r#"# PowerShell 5.1. Guest lead own mode offers loop.
while (-not (Test-Path .sommelier/stop)) {
  sommelier offers --top 5 --wait 600
  $rc = $LASTEXITCODE
  if ($rc -eq 0) { exit 0 }
  elseif ($rc -eq 4) { sommelier sync }
  elseif ($rc -eq 5) { Start-Sleep -Seconds ([int](sommelier quota wait-secs)) }
  elseif ($rc -eq 6) { Start-Sleep -Seconds 60 }
  else { Start-Sleep -Seconds 30 }
}
"#;
