-- Example script for the uw-combat module. Real location, beside the YAML
-- that lists it, in the config dir:
--   Linux:   ~/.config/mudular/modules/uw-combat.lua
--   macOS:   ~/Library/Application Support/mudular/modules/uw-combat.lua
--   Windows: %APPDATA%\mudular\config\modules\uw-combat.lua
-- in `scripts:`.
--
-- Everything a script can do arrives through the `mud` table: there is no
-- io, os, or require, and a hook that runs too long is aborted rather than
-- stalling the character it belongs to.
--
-- Variables are shared with the rules: `mud.get`/`mud.set` read and write
-- the same store as a YAML `variables:` block and a rule's `set:`, so a
-- trigger and a hook can hand work to each other.

-- Keep the potion count honest across a whole session — state that lives
-- between lines is the first thing YAML rules cannot express.
local potions = 0

mud.on_line(function(line)
  local drank = line:match("^You quaff a (%w+) potion")
  if drank then
    potions = potions + 1
    mud.echo("** " .. potions .. " potions this session")
  end
end)

-- React to a GMCP vital with arithmetic and a threshold the pattern never
-- sees. `mud.data` reads the same server-data store `${...}` templates and
-- `when:` guards use, keyed by the dotted GMCP path.
mud.on_gmcp(function(package)
  if package ~= "Char.Vitals" then
    return
  end
  local hp = tonumber(mud.data("Char.Vitals.hp"))
  local max = tonumber(mud.data("Char.Vitals.maxhp"))
  if hp and max and max > 0 and hp / max < 0.3 then
    mud.send("quaff heal")
  end
end)

-- Called by a rule's `script:` action rather than by an event. The
-- arguments are the matched line and its captures: numbered groups at
-- caps[1], caps[2], ..., named groups under their own names.
local kills = 0

function on_death(_, caps)
  kills = kills + 1
  mud.echo("** " .. caps.victim .. " down (" .. kills .. " this session)")
end

-- Greet the MUD in this character's own words once the connection is up.
mud.on_connect(function()
  mud.send("say " .. (mud.get("greeting") or "hello"))
end)
