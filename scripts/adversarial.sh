#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

campaign_catalog=(
  overall
  storage-durability
  ingest-retention
  vector-execution
  vamana-graph
  metadata-filter-planner
  fts
  hybrid-fusion
  tiering-maintenance
  lifecycle-accounting
  diagnostics-health
  ffi-bindings
)

usage() {
  echo "usage: scripts/adversarial.sh {list|episode|replay|smoke|campaign} [options]" >&2
}

fail() {
  echo "adversarial: $*" >&2
  exit 2
}

valid_campaigns() {
  local joined=""
  local key
  for key in "${campaign_catalog[@]}"; do
    if [[ -n "$joined" ]]; then
      joined="$joined, "
    fi
    joined="$joined$key"
  done
  echo "$joined"
}

campaign_is_valid() {
  local requested="$1"
  local key
  for key in "${campaign_catalog[@]}"; do
    if [[ "$requested" == "$key" ]]; then
      return 0
    fi
  done
  return 1
}

require_value() {
  local option="$1"
  local count="$2"
  if (( count < 2 )); then
    fail "$option requires a value"
  fi
}

if (( $# == 0 )); then
  usage
  exit 2
fi

subcommand="$1"
shift
case "$subcommand" in
  list)
    if (( $# != 0 )); then
      fail "list accepts no options"
    fi
    printf '%s\n' "${campaign_catalog[@]}"
    exit 0
    ;;
  episode|replay|smoke|campaign) ;;
  *)
    usage
    fail "unknown subcommand: $subcommand"
    ;;
esac

campaign_environment_set=0
if [[ -n "${ZE_ADV_CAMPAIGN+x}" ]]; then
  campaign_environment_set=1
fi
campaign="${ZE_ADV_CAMPAIGN:-overall}"
seed="${ZE_ADV_SEED:-0}"
start_seed="${ZE_ADV_START_SEED:-${ZE_ADV_CAMPAIGN_START_SEED:-0}}"
profile="${ZE_ADV_PROFILE:-none}"
qualification="${ZE_ADV_QUALIFICATION:-exploratory}"
min_seconds="${ZE_ADV_MIN_SECONDS:-28800}"
min_episodes="${ZE_ADV_MIN_EPISODES:-10000}"
retain_successful="${ZE_ADV_RETAIN_SUCCESSFUL:-256}"
artifacts="${ZE_ADV_ARTIFACTS:-target/adversarial}"
replay_dir="${ZE_ADV_REPLAY_DIR:-}"

campaign_option=0
seed_option=0
start_seed_option=0
profile_option=0
qualification_option=0
min_seconds_option=0
min_episodes_option=0
retain_option=0
artifacts_option=0
replay_option=0

while (( $# > 0 )); do
  option="$1"
  case "$option" in
    --campaign)
      require_value "$option" "$#"
      (( campaign_option == 0 )) || fail "--campaign may be specified exactly once"
      campaign="$2"
      campaign_option=1
      shift 2
      ;;
    --seed)
      require_value "$option" "$#"
      (( seed_option == 0 )) || fail "--seed may be specified once"
      seed="$2"
      seed_option=1
      shift 2
      ;;
    --start-seed)
      require_value "$option" "$#"
      (( start_seed_option == 0 )) || fail "--start-seed may be specified once"
      start_seed="$2"
      start_seed_option=1
      shift 2
      ;;
    --profile)
      require_value "$option" "$#"
      (( profile_option == 0 )) || fail "--profile may be specified once"
      profile="$2"
      profile_option=1
      shift 2
      ;;
    --qualification)
      require_value "$option" "$#"
      (( qualification_option == 0 )) || fail "--qualification may be specified once"
      qualification="$2"
      qualification_option=1
      shift 2
      ;;
    --min-seconds)
      require_value "$option" "$#"
      (( min_seconds_option == 0 )) || fail "--min-seconds may be specified once"
      min_seconds="$2"
      min_seconds_option=1
      shift 2
      ;;
    --min-episodes)
      require_value "$option" "$#"
      (( min_episodes_option == 0 )) || fail "--min-episodes may be specified once"
      min_episodes="$2"
      min_episodes_option=1
      shift 2
      ;;
    --retain-successful)
      require_value "$option" "$#"
      (( retain_option == 0 )) || fail "--retain-successful may be specified once"
      retain_successful="$2"
      retain_option=1
      shift 2
      ;;
    --artifacts)
      require_value "$option" "$#"
      (( artifacts_option == 0 )) || fail "--artifacts may be specified once"
      artifacts="$2"
      artifacts_option=1
      shift 2
      ;;
    --replay-dir)
      require_value "$option" "$#"
      (( replay_option == 0 )) || fail "--replay-dir may be specified once"
      replay_dir="$2"
      replay_option=1
      shift 2
      ;;
    --*) fail "unknown option: $option" ;;
    *) fail "unexpected argument: $option" ;;
  esac
done

[[ "$campaign" != ,* && "$campaign" != *, && "$campaign" != *,,* ]] \
  || fail "campaign lists may not contain empty entries"
IFS=',' read -r -a campaigns <<< "$campaign"
(( ${#campaigns[@]} > 0 )) || fail "at least one campaign is required"
seen_campaigns="|"
for selected_campaign in "${campaigns[@]}"; do
  campaign_is_valid "$selected_campaign" \
    || fail "unknown campaign '$selected_campaign'; valid campaigns: $(valid_campaigns)"
  [[ "$seen_campaigns" != *"|$selected_campaign|"* ]] \
    || fail "campaign '$selected_campaign' was selected more than once"
  seen_campaigns="$seen_campaigns$selected_campaign|"
done
if (( ${#campaigns[@]} > 1 )); then
  [[ "$subcommand" == "smoke" || "$subcommand" == "campaign" ]] \
    || fail "$subcommand requires exactly one campaign"
  for selected_campaign in "${campaigns[@]}"; do
    [[ "$selected_campaign" != "overall" ]] \
      || fail "overall may not be combined with feature campaigns"
  done
fi

case "$profile" in
  io) profile="io-errors" ;;
  none|io-errors|content|crash|disk|clock|full) ;;
  *) fail "invalid profile '$profile'; expected none|io-errors|content|crash|disk|clock|full" ;;
esac

case "$qualification" in
  exploratory|release) ;;
  *) fail "invalid qualification '$qualification'; expected exploratory|release" ;;
esac

for numeric_name in seed start_seed min_seconds min_episodes retain_successful; do
  numeric_value="${!numeric_name}"
  [[ "$numeric_value" =~ ^[0-9]+$ ]] \
    || fail "--${numeric_name//_/-} must be an unsigned integer, got '$numeric_value'"
done

case "$subcommand" in
  episode)
    (( replay_option == 0 )) || fail "--replay-dir is valid only for replay"
    (( start_seed_option == 0 )) || fail "--start-seed is valid only for smoke or campaign"
    (( min_seconds_option == 0 && min_episodes_option == 0 && retain_option == 0 )) \
      || fail "campaign thresholds are valid only for campaign"
    ;;
  replay)
    [[ -n "$replay_dir" ]] || fail "replay requires --replay-dir or ZE_ADV_REPLAY_DIR"
    (( start_seed_option == 0 )) || fail "--start-seed is valid only for smoke or campaign"
    (( min_seconds_option == 0 && min_episodes_option == 0 && retain_option == 0 )) \
      || fail "campaign thresholds are valid only for campaign"
    ;;
  smoke)
    (( replay_option == 0 )) || fail "--replay-dir is valid only for replay"
    (( min_seconds_option == 0 && min_episodes_option == 0 && retain_option == 0 )) \
      || fail "campaign thresholds are valid only for campaign"
    ;;
  campaign)
    (( replay_option == 0 )) || fail "--replay-dir is valid only for replay"
    if [[ "$qualification" == "release" ]]; then
      (( min_seconds >= 28800 )) \
        || fail "release qualification must run at least 28800 seconds"
      (( min_episodes >= 10000 )) \
        || fail "release qualification must run at least 10000 episodes"
    fi
    ;;
esac

if [[ "$artifacts" != /* ]]; then
  artifacts="$repo_root/$artifacts"
fi
if [[ -n "$replay_dir" && "$replay_dir" != /* ]]; then
  replay_dir="$repo_root/$replay_dir"
fi

export ZE_ADV_CAMPAIGN="$campaign"
export ZE_ADV_QUALIFICATION="$qualification"
export ZE_ADV_SEED="$seed"
export ZE_ADV_CAMPAIGN_START_SEED="$start_seed"
export ZE_ADV_PROFILE="$profile"
export ZE_ADV_MIN_SECONDS="$min_seconds"
export ZE_ADV_MIN_EPISODES="$min_episodes"
export ZE_ADV_RETAIN_SUCCESSFUL="$retain_successful"
export ZE_ADV_ARTIFACTS="$artifacts"
if [[ -n "$replay_dir" ]]; then
  export ZE_ADV_REPLAY_DIR="$replay_dir"
fi
infer_replay_campaign=0
if [[ "$subcommand" == "replay" ]] \
  && (( campaign_environment_set == 0 && campaign_option == 0 )); then
  unset ZE_ADV_CAMPAIGN
  infer_replay_campaign=1
fi

if [[ "${ZE_ADV_CLI_TEST:-0}" == "1" ]]; then
  campaign_label="campaign=$campaign"
  if (( ${#campaigns[@]} > 1 )); then
    campaign_label="campaigns=$campaign"
  fi
  echo "subcommand=$subcommand $campaign_label seed=$seed profile=$profile qualification=$qualification start_seed=$start_seed min_seconds=$min_seconds min_episodes=$min_episodes retain_successful=$retain_successful artifacts=$artifacts replay_dir=$replay_dir"
  exit 0
fi

cd "$repo_root"
base_artifacts="$artifacts"
for selected_campaign in "${campaigns[@]}"; do
  if (( infer_replay_campaign == 0 )); then
    export ZE_ADV_CAMPAIGN="$selected_campaign"
  else
    unset ZE_ADV_CAMPAIGN
  fi
  if (( ${#campaigns[@]} > 1 )); then
    export ZE_ADV_ARTIFACTS="$base_artifacts/$selected_campaign"
  else
    export ZE_ADV_ARTIFACTS="$base_artifacts"
  fi
  case "$subcommand" in
    episode)
      cargo test -p zeppelin-embed-workspace-tests \
        --test adversarial_tests run -- --ignored --exact --nocapture
      ;;
    replay)
      cargo test -p zeppelin-embed-workspace-tests \
        --test adversarial_tests replay -- --ignored --exact --nocapture
      ;;
    smoke)
      cargo test -p zeppelin-embed-workspace-tests \
        --test adversarial_tests smoke -- --exact --nocapture
      ;;
    campaign)
      cargo test --release -p zeppelin-embed-workspace-tests \
        --test adversarial_tests campaign -- --ignored --exact --nocapture
      ;;
  esac
done
