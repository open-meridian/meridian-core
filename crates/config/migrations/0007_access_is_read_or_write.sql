-- Access to a plugin is read or write, the same for every plugin, and a
-- plugin declares no tags (decisions/026). An access entry names a plugin and
-- a level, and nothing else.
--
-- An access group's entries naming one plugin, through whichever tags, become
-- one entry at the highest level any of them had: `read` on one tag and
-- `write` on another is `write`. That is what the person held on the plugin's
-- accounts already, since write on any tag reached them. Each group keeps its
-- plugins in the order each first appeared.
CREATE TEMPORARY TABLE config_access_entry_collapsed ON COMMIT DROP AS
SELECT access_group_id,
       plugin_instance_id,
       max(level)    AS level,
       min(position) AS first_position
  FROM config_access_entry
 GROUP BY access_group_id, plugin_instance_id;

DELETE FROM config_access_entry;
ALTER TABLE config_access_entry DROP COLUMN IF EXISTS tag;

INSERT INTO config_access_entry (access_group_id, position, plugin_instance_id, level)
SELECT access_group_id,
       (row_number() OVER (PARTITION BY access_group_id ORDER BY first_position) - 1)::integer,
       plugin_instance_id,
       level
  FROM config_access_entry_collapsed;

-- The tags a plugin was launched and reported with, which nothing reads now.
ALTER TABLE config_known_plugin DROP COLUMN IF EXISTS tags;
ALTER TABLE config_plugin_version DROP COLUMN IF EXISTS tags;
ALTER TABLE config_plugin_launch DROP COLUMN IF EXISTS tags;
