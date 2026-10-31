#!/bin/sh
# Stands in for Claude Code in the release smoke. It speaks the stream-json a real run writes and
# does what the mail teaching tells an agent to do, with the `dudes` on its path: it asks a
# teammate, answers a request, and reports an answer back.
case "$*" in
  '--version') printf 'smoke stand-in\n'; exit 0 ;;
  'auth status'|'login status') exit 0 ;;
esac

session=11111111-1111-4111-8111-111111111111
previous=
task=
for arg in "$@"; do
  case "$previous" in
    --session-id|--resume) session=$arg ;;
  esac
  previous=$arg
  task=$arg
done

case "$task" in
  *' new mail]'*)
    mail=$(dudes mail read) || exit 1
    id=$(printf '%s\n' "$mail" | sed -n 's/^mail //p' | head -n 1)
    act=$(printf '%s\n' "$mail" | sed -n 's/^act //p' | head -n 1)
    if [ "$act" = request ]; then
      # The reply command is only known from the teaching in the task, as it is to a real agent.
      if printf '%s' "$task" | grep -q '`dudes mail send --reply-to <id> --act message --body <text>`'; then
        dudes mail send --reply-to "$id" --act message --body 'The answer is 4' || exit 1
        reply='I answered the request.'
      else
        reply='I was not taught how to reply.'
      fi
    else
      reply="Bruno says: $(printf '%s\n' "$mail" | tail -n 1)"
    fi
    dudes mail done "$id" || exit 1
    ;;
  *'Ask Bruno'*)
    to=$(dudes team | awk '$2 == "Bruno" { print $1 }')
    [ -n "$to" ] || exit 1
    dudes mail send --to "$to" --act request --subject 'What is 2+2?' --body 'What is 2+2?' || exit 1
    reply='I asked Bruno.'
    ;;
  *) reply='stand-in reply' ;;
esac

printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"$session\",\"model\":\"smoke-stand-in\"}"
printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"model\":\"smoke-stand-in\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"$reply\"}],\"usage\":{\"input_tokens\":10,\"output_tokens\":2}},\"session_id\":\"$session\"}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1,\"result\":\"$reply\",\"stop_reason\":\"end_turn\",\"terminal_reason\":\"completed\",\"session_id\":\"$session\",\"total_cost_usd\":0,\"usage\":{\"input_tokens\":10,\"output_tokens\":2},\"modelUsage\":{}}"
