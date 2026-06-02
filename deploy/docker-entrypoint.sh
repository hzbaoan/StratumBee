#!/bin/sh
set -eu

APP_USER="${STRATUMBEE_USER:-stratumbee}"
APP_GROUP="${STRATUMBEE_GROUP:-stratumbee}"
APP_HOME="/opt/stratumbee"
DEFAULT_CONFIG="$APP_HOME/config/stratumbee.toml"

if [ "${1:-}" = "" ]; then
    set -- stratumbee --config "$DEFAULT_CONFIG"
elif [ "${1#-}" != "$1" ]; then
    set -- stratumbee "$@"
fi

if [ "$(id -u)" -ne 0 ]; then
    exec "$@"
fi

if [ "${1:-}" != "stratumbee" ]; then
    exec "$@"
fi

config_path="$DEFAULT_CONFIG"
expect_config_value=0
for arg in "$@"; do
    if [ "$expect_config_value" = "1" ]; then
        config_path="$arg"
        expect_config_value=0
        continue
    fi

    case "$arg" in
        --config)
            expect_config_value=1
            ;;
        --config=*)
            config_path="${arg#--config=}"
            ;;
    esac
done

toml_value() {
    key="$1"
    file="$2"
    sed -n "s/^[[:space:]]*$key[[:space:]]*=[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$file" | tail -n 1
}

resolve_app_path() {
    path="$1"
    case "$path" in
        /*) printf '%s\n' "$path" ;;
        *) printf '%s/%s\n' "$APP_HOME" "$path" ;;
    esac
}

prepare_public_key() {
    path="$1"
    [ -n "$path" ] || return 0
    resolved="$(resolve_app_path "$path")"
    [ -e "$resolved" ] || return 0

    chown "$APP_USER:$APP_GROUP" "$resolved" 2>/dev/null || true
    chmod 0644 "$resolved" 2>/dev/null || true
}

prepare_secret_key() {
    path="$1"
    [ -n "$path" ] || return 0
    resolved="$(resolve_app_path "$path")"
    [ -e "$resolved" ] || return 0

    chown "$APP_USER:$APP_GROUP" "$resolved" || {
        echo "failed to chown SV2 authority secret key: $resolved" >&2
        exit 1
    }
    chmod 0600 "$resolved" || {
        echo "failed to chmod 600 SV2 authority secret key: $resolved" >&2
        exit 1
    }
}

if [ -f "$config_path" ]; then
    public_key_path="$(toml_value authority_public_key_path "$config_path")"
    secret_key_path="$(toml_value authority_secret_key_path "$config_path")"
    prepare_public_key "$public_key_path"
    prepare_secret_key "$secret_key_path"
fi

exec gosu "$APP_USER:$APP_GROUP" "$@"
