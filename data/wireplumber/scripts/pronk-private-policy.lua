-- SPDX-License-Identifier: MIT
--
-- Extend whichever permission manager WirePlumber selected for an ordinary,
-- sandboxed, or portal client.  Replacing those managers would discard their
-- existing security decisions, especially portal camera authorization.

local cutils = require ("common-utils")
local log = Log.open_topic ("pronk-policy")
local extended_managers = setmetatable ({}, { __mode = "k" })
local direct_managers = {}

local function private_deny_rules ()
  return Json.Array {
    Json.Object {
      ["matches"] = Json.Array {
        Json.Object {
          -- A regexp match also requires the property to be present. Deny
          -- unknown marker versions here; only the backend's production PM
          -- contains the exact-version allow rule.
          ["api.pronk.private"] = "~.*",
        },
      },
      ["actions"] = Json.Object {
        ["set-permissions"] = "-",
      },
    },
  }
end

local function add_private_deny (manager)
  if extended_managers[manager] then
    return
  end

  -- Rule matching examines both registry-global and fully loaded PipeWire
  -- object properties. Custom stream properties are not guaranteed to be
  -- copied into the registry-global property set.
  manager:add_rules_match (private_deny_rules ())
  extended_managers[manager] = true
end

local function manager_for_direct_permissions (permissions)
  local manager = direct_managers[permissions]
  if manager ~= nil then
    return manager
  end

  manager = PermissionManager ()
  manager:set_default_permissions (permissions)
  manager:set_core_permissions (permissions)
  add_private_deny (manager)
  direct_managers[permissions] = manager
  return manager
end

SimpleEventHook {
  name = "client/find-pronk-private-access",
  before = "client/apply-access",
  after = {
    "client/find-config-access",
    "client/find-flatpak-access",
    "client/find-snap-access",
    "client/find-portal-access",
    "client/find-default-access",
  },
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-access" },
    },
  },
  execute = function (event)
    local client = event:get_subject ()

    -- WirePlumber must retain full graph visibility in order to evaluate the
    -- per-object matches and create the selected links. Do not leave it
    -- attached to the shared default manager: tightening that manager for an
    -- ordinary client would otherwise make the policy daemon lose the very
    -- object it needs to keep tracking.
    if client:get_property ("wireplumber.daemon") ~= nil then
      event:set_data ("permission-manager", nil)
      event:set_data ("default-permissions", "all")
      return
    end

    local access = cutils.get_client_access (client.properties)
    if access == "pronk-core" or access == "pronk-backend" then
      local expected = access == "pronk-core"
          and "pronk-core-policy-v1" or "pronk-backend-policy-v1"
      local selected = event:get_data ("permission-manager")
      local effective = event:get_data ("effective-access")

      if selected == nil or effective ~= expected then
        -- A missing, shadowed, or malformed role rule must not fall through to
        -- WirePlumber's ordinary all-access manager.
        log:warning (client,
            "blocking Pronk role because its versioned permission manager was not selected")
        event:set_data ("default-permissions", "-")
      end
      return
    end

    local direct = event:get_data ("default-permissions")
    if direct ~= nil then
      -- Preserve an administrator's direct fallback grant while adding the
      -- private-node exception.  apply-access gives direct permissions
      -- precedence, so clear that slot after installing the equivalent PM.
      event:set_data ("default-permissions", nil)
      event:set_data ("permission-manager",
          manager_for_direct_permissions (direct))
      return
    end

    local manager = event:get_data ("permission-manager")
    if manager == nil then
      log:warning (client,
          "blocking client because no permission source was selected")
      event:set_data ("default-permissions", "-")
      return
    end
    add_private_deny (manager)
  end
}:register ()
