# |source| this file
#
# Common utilities shared by other scripts in this directory
#
# The following directive disable complaints about unused variables in this
# file:
# shellcheck disable=2034
# shellcheck shell=bash
#

here="$(cd "$(dirname "${BASH_SOURCE[0]}")"/.. || exit 1; pwd)"
# shellcheck disable=SC1091
# shellcheck source=../net/common.sh
source "$here"/net/common.sh

prebuild=
if [[ $1 = "--prebuild" ]]; then
  prebuild=true
fi

if [[ -n $USE_INSTALL || ! -f "$SOLANA_ROOT"/Cargo.toml ]]; then
  solana_program() {
    declare program="$1"
    if [[ -z $program ]]; then
      printf "solana"
    else
      if [[ $program == "validator" || $program == "ledger-tool" || $program == "watchtower" || $program == "install" ]]; then
        printf "agave-%s" "$program"
      else
        printf "solana-%s" "$program"
      fi
    fi
  }
else
  solana_program() {
    declare program="$1"
    declare crate="$program"
    declare manifest_path
    if [[ $program == "ledger-tool" ]]; then
      manifest_path="--manifest-path $here/dev-bins/Cargo.toml"
    fi
    if [[ -z $program ]]; then
      crate="cli"
      program="solana"
    elif [[ $program == "validator" || $program == "ledger-tool" || $program == "watchtower" || $program == "install" ]]; then
      program="agave-$program"
    else
      program="solana-$program"
    fi

    if [[ -n $CARGO_BUILD_PROFILE ]]; then
      profile_arg="--profile $CARGO_BUILD_PROFILE"
    fi

    # Prebuild binaries so that CI sanity check timeout doesn't include build time
    if [[ $prebuild ]]; then
      (
        set -x
        # shellcheck disable=SC2086 # Don't want to double quote
        cargo $CARGO_TOOLCHAIN build $manifest_path $profile_arg --bin $program
      )
    fi

    printf "cargo $CARGO_TOOLCHAIN run $manifest_path $profile_arg --bin %s %s -- " "$program"
  }
fi

solana_transaction_bench=${SOLANA_TRANSACTION_BENCH:-solana-transaction-bench}
solana_faucet=$(solana_program faucet)
agave_validator=$(solana_program validator)
solana_genesis=$(solana_program genesis)
solana_gossip=$(solana_program gossip)
solana_keygen=$(solana_program keygen)
solana_ledger_tool=$(solana_program ledger-tool)
solana_cli=$(solana_program)

export RUST_BACKTRACE=1

default_arg() {
  declare name=$1
  declare value=$2

  for arg in "${args[@]}"; do
    if [[ $arg = "$name" ]]; then
      return
    fi
  done

  if [[ -n $value ]]; then
    args+=("$name" "$value")
  else
    args+=("$name")
  fi
}

replace_arg() {
  declare name=$1
  declare value=$2

  default_arg "$name" "$value"

  declare index=0
  for arg in "${args[@]}"; do
    index=$((index + 1))
    if [[ $arg = "$name" ]]; then
      args[$index]="$value"
    fi
  done
}

EPOCH_REWARDS_SYSVAR=SysvarEpochRewards1111111111111111111111111
EPOCH_REWARDS_ACTIVE_ERROR="epoch rewards period is active"

# Echoes "true" while the cluster is distributing partitioned epoch rewards,
# "false" otherwise. Echoes nothing if the state could not be determined.
epoch_rewards_active() {
  declare rpc_url=$1

  if ! command -v curl > /dev/null 2>&1 || ! command -v jq > /dev/null 2>&1; then
    return 0
  fi

  declare account_json
  if ! account_json=$(curl -sf -X POST -H 'Content-Type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"$EPOCH_REWARDS_SYSVAR\",{\"encoding\":\"jsonParsed\"}]}" \
    "$rpc_url"); then
    return 0
  fi

  # The sysvar does not exist before the first epoch boundary, which also means
  # no rewards are being distributed
  echo "$account_json" | jq -r 'if .result.value == null then false else .result.value.data.parsed.info.active end' 2> /dev/null
}

# Blocks until the epoch rewards period is over, so that stake instructions are
# not rejected outright. Best effort: gives up quietly, the caller is expected
# to also handle the failure.
wait_for_epoch_rewards_inactive() {
  declare rpc_url=$1
  declare max_attempts=$2
  declare delay_seconds=$3

  declare attempt
  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    if [[ $(epoch_rewards_active "$rpc_url") != true ]]; then
      return 0
    fi

    echo "Epoch rewards period is active; waiting $delay_seconds seconds ($attempt/$max_attempts)" >&2
    sleep "$delay_seconds"
  done
}

# Runs a command, retrying only while it fails because the epoch rewards period
# is active. The rewards period can start between a `wait_for_epoch_rewards_inactive`
# check and the transaction landing, so the retry is what actually closes the race.
retry_while_epoch_rewards_active() {
  declare max_attempts=$1
  declare delay_seconds=$2
  shift 2

  declare attempt=1
  declare output
  declare status
  while true; do
    output=$("$@" 2>&1)
    status=$?
    echo "$output" >&2

    if ((status == 0)); then
      return 0
    fi

    if [[ $output != *"$EPOCH_REWARDS_ACTIVE_ERROR"* ]] || ((attempt >= max_attempts)); then
      return "$status"
    fi

    echo "Command failed; retrying in $delay_seconds seconds ($attempt/$max_attempts)" >&2
    sleep "$delay_seconds"
    attempt=$((attempt + 1))
  done
}
