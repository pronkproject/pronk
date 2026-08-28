-- SPDX-License-Identifier: MIT
--
-- This object exists only while this WirePlumber instance and the complete
-- Pronk access-policy feature graph are alive. Producers use its versioned
-- object name as the publication gate; the property makes that version easy
-- to inspect without encoding policy details in producer code.

Script.async_activation = true

-- Deliberately global: WirePlumber's Lua bindings release a local object once
-- the activation closure goes away, which would immediately remove the marker
-- from PipeWire. The script global anchors it until policy shutdown.
pronk_policy_metadata = ImplMetadata ("pronk-policy-v1")
pronk_policy_metadata:activate (Features.ALL, function (object, error)
  if error then
    Script:finish_activation_with_error (
        "failed to publish the Pronk policy marker: " .. tostring (error))
    return
  end

  object:set (0, "pronk.policy.version", "Spa:Int", "1")
  Script:finish_activation ()
end)
