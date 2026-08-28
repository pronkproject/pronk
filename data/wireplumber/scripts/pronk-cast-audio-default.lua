-- Select a cast display's audio output when that display becomes available.
--
-- This deliberately changes only default.audio.sink.  The configured default
-- remains the user's normal output, so removing the display restores that
-- preference and selecting another output while casting remains authoritative.

local log = Log.open_topic ("s-pronk-cast-audio")

local CAST_SINK_MARKER = "api.pronk.castkms.audio-sink"
local CAST_SINK_VERSION = "v1"
local OUTPUT_INDEX = "api.pronk.castkms.output-index"

local available_cast_sinks = {}
local automatically_selected_sink = nil

local function cast_sink_candidates (event)
  local available_nodes = event:get_data ("available-nodes")
  available_nodes = available_nodes and available_nodes:parse ()
  if not available_nodes then
    return {}, {}
  end

  local candidates = {}
  local names = {}
  for _, properties in ipairs (available_nodes) do
    local name = properties["node.name"]
    local index = tonumber (properties[OUTPUT_INDEX])
    if properties[CAST_SINK_MARKER] == CAST_SINK_VERSION and
        type (name) == "string" and name ~= "" and
        index and index >= 0 and index <= 7 and index % 1 == 0 then
      table.insert (candidates, { name = name, index = index })
      names[name] = true
    end
  end

  table.sort (candidates, function (left, right)
    if left.index == right.index then
      return left.name < right.name
    end
    return left.index < right.index
  end)
  return candidates, names
end

local function select_sink (event, name, reason)
  log:info (reason .. ": " .. name)
  event:set_data ("selected-node", name)
  automatically_selected_sink = name
end

SimpleEventHook {
  name = "pronk/default-cast-audio-sink",
  after = {
    "default-nodes/find-best-default-node",
    "default-nodes/find-selected-default-node",
    "default-nodes/find-stored-default-node",
  },
  before = { "default-nodes/apply-default-node" },
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-default-node" },
      Constraint { "default-node.type", "=", "audio.sink" },
    },
  },
  execute = function (event)
    local candidates, current_names = cast_sink_candidates (event)

    -- A display that has just acquired an available audio route takes over
    -- once.  Stable slot ordering makes simultaneous attachment deterministic.
    for _, candidate in ipairs (candidates) do
      if not available_cast_sinks[candidate.name] then
        select_sink (event, candidate.name, "selecting newly available cast audio sink")
        available_cast_sinks = current_names
        return
      end
    end

    -- Keep an automatic choice through unrelated default-node rescans.  A
    -- configured-default metadata event clears this state before its rescan,
    -- allowing an explicit user selection to win instead.
    if automatically_selected_sink and current_names[automatically_selected_sink] then
      event:set_data ("selected-node", automatically_selected_sink)
    elseif automatically_selected_sink and #candidates > 0 then
      select_sink (event, candidates[1].name, "replacing removed cast audio sink")
    else
      automatically_selected_sink = nil
    end

    available_cast_sinks = current_names
  end,
}:register ()

-- Pronk never writes the configured default.  A change here therefore means
-- the user (or another desktop policy) selected an output and must end the
-- automatic choice even when it names the same pre-cast output as before.
SimpleEventHook {
  name = "pronk/notice-configured-audio-sink",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "metadata-changed" },
      Constraint { "metadata.name", "=", "default" },
      Constraint { "event.subject.key", "=", "default.configured.audio.sink" },
    },
  },
  execute = function ()
    automatically_selected_sink = nil
  end,
}:register ()
