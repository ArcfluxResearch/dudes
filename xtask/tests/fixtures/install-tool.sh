#!/bin/sh
exec /bin/sh "${0%/*}/.${0##*/}.sh" "$@"
