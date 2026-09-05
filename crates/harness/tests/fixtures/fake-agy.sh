#!/bin/sh
# Fake Antigravity CLI (agy) for komet-harness tests.

emit() { printf '%s\n' "$1"; }

if [ "$1" = "models" ]; then
    emit "gemini-3.8-flash-high     Gemini 3.8 Flash (High)"
    emit "gemini-3.7-flash-high     Gemini 3.7 Flash (High)"
    emit "gemini-3.1-pro-high       Gemini 3.1 Pro (High)"
    emit "claude-sonnet-4-6         Claude Sonnet 4.6 (Thinking)"
    exit 0
fi

# Verify arguments for normal run
has_skip_perms=0
has_add_dir=0
prompt=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dangerously-skip-permissions)
            has_skip_perms=1
            shift
            ;;
        --add-dir)
            has_add_dir=1
            shift 2
            ;;
        -p)
            prompt="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

case "$prompt" in
    *scenario:happy*)
        emit '{"event":"init","conversation_id":"fake-convo-1","init":{"model":"gemini-3.8-flash","tools":["list_dir","run_command"],"cwd":"/test/project"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-1","step_index":0,"state":"DONE","step_type":"user_input"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-1","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"Hello from Antigravity!"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-1","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"\n","usage":{"input_tokens":100,"output_tokens":20,"thinking_tokens":10,"cache_read_tokens":50,"total_tokens":120}}}'
        emit '{"event":"result","result":{"conversation_id":"fake-convo-1","status":"SUCCESS","response":"Hello from Antigravity!\n","usage":{"input_tokens":100,"output_tokens":20,"thinking_tokens":10,"cache_read_tokens":50,"total_tokens":120}}}'
        ;;
    *scenario:tool_lifecycle*)
        emit '{"event":"init","conversation_id":"fake-convo-tools","init":{"model":"gemini-3.8-flash","tools":["list_dir"],"cwd":"/test/project"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-tools","step_index":1,"state":"ACTIVE","step_type":"tool","tool_name":"list_dir","tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/test/project"}}}}'
        # agy emits the tool frame again on completion with output
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-tools","step_index":1,"state":"DONE","step_type":"tool","tool_name":"list_dir","tool_info":{"name":"list_dir","parameters":{"DirectoryPath":"/test/project"},"output":"Cargo.toml\nsrc/"}}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-tools","step_index":2,"state":"ACTIVE","step_type":"agent_response","text_delta":"Found files"}}'
        emit '{"event":"result","result":{"conversation_id":"fake-convo-tools","status":"SUCCESS","response":"Found files"}}'
        ;;
    *scenario:tool_error*)
        emit '{"event":"init","conversation_id":"fake-convo-err","init":{"model":"gemini-3.8-flash","tools":["run_command"],"cwd":"/test/project"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-err","step_index":1,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"bad_cmd"}}}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-convo-err","step_index":1,"state":"ERROR","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"bad_cmd"},"error":{"type":"EXEC_ERROR","message":"bad_cmd: not found"}}}}'
        emit 'jetski: execution failed'
        emit '{"event":"result","result":{"conversation_id":"fake-convo-err","status":"CANCELED","response":""}}'
        ;;
    *scenario:verify_flags*)
        if [ "$has_skip_perms" -ne 1 ]; then
            echo "Missing --dangerously-skip-permissions" >&2
            exit 2
        fi
        if [ "$has_add_dir" -ne 1 ]; then
            echo "Missing --add-dir" >&2
            exit 3
        fi
        emit '{"event":"init","conversation_id":"fake-flags","init":{"model":"gemini-3.8-flash","tools":[],"cwd":"/test/project"}}'
        emit '{"event":"step_update","step_update":{"conversation_id":"fake-flags","step_index":1,"state":"ACTIVE","step_type":"agent_response","text_delta":"flags ok"}}'
        emit '{"event":"result","result":{"conversation_id":"fake-flags","status":"SUCCESS","response":"flags ok"}}'
        ;;
    *scenario:crash*)
        echo "Fatal runtime error in agy" >&2
        exit 1
        ;;
    *scenario:hang*)
        sleep 30
        ;;
    *)
        emit '{"event":"init","conversation_id":"fake-default","init":{"model":"default","tools":[],"cwd":"/tmp"}}'
        emit '{"event":"result","result":{"conversation_id":"fake-default","status":"SUCCESS","response":"ok"}}'
        ;;
esac
