-- Compare ovfs with the Bareos restore query on the catalog of generate.sql.
--
--   podman exec -i -u postgres postgres-dev psql -X -d ovfs_bench < ovfs/bench/parity.sql
--
-- Expected: bareos = tree = 1230502, ls_rows = 1230501 (all but the root),
-- every difference column 0.

\set ON_ERROR_STOP on

-- Bareos 25.1.2: select_recent_version_with_basejob_and_delta, then FileIndex > 0
CREATE TEMP TABLE bareos AS
SELECT p.path, t.name, t.jobid, t.fileindex FROM (
    SELECT DISTINCT ON (f.name, f.pathid, f.deltaseq) f.jobid, f.fileindex, f.pathid, f.name
    FROM file f JOIN job USING (jobid) WHERE f.jobid IN (1, 2)
    ORDER BY f.name, f.pathid, f.deltaseq, jobtdate DESC) t
JOIN path p USING (pathid) WHERE t.fileindex > 0;

-- ovfs.tree, flattened to (path, name, jobid, fileindex); directories have name ''
CREATE TEMP TABLE tree_json AS SELECT ovfs.tree(ARRAY[1, 2], '/', 1000, 2000000) AS j;
CREATE TEMP TABLE tree_flat AS
WITH RECURSIVE n(node, path) AS (
    SELECT j, j->>'path' FROM tree_json
    UNION ALL
    SELECT c, CASE WHEN c->>'type' = 'directory' THEN c->>'path' ELSE n.path END
    FROM n, json_array_elements(n.node->'children') c WHERE n.node->>'type' = 'directory')
SELECT path, CASE WHEN node->>'type' = 'directory' THEN '' ELSE node->>'name' END AS name,
       (node->>'jobid')::int AS jobid, (node->>'fileindex')::int AS fileindex
FROM n WHERE node->>'fileindex' IS NOT NULL;

-- ovfs.ls over every directory
CREATE TEMP TABLE ls_flat AS
WITH RECURSIVE d(path) AS (
    SELECT '/'::text
    UNION ALL
    SELECT l.path FROM d, ovfs.ls(ARRAY[1, 2], d.path) l WHERE l.kind = 'directory')
SELECT d.path, l.name, l.kind, l.jobid, l.fileindex FROM d, ovfs.ls(ARRAY[1, 2], d.path) l;

SELECT (SELECT count(*) FROM bareos) AS bareos,
       (SELECT count(*) FROM tree_flat) AS tree,
       (SELECT count(*) FROM (SELECT * FROM bareos EXCEPT ALL SELECT * FROM tree_flat) x) AS only_bareos,
       (SELECT count(*) FROM (SELECT * FROM tree_flat EXCEPT ALL SELECT * FROM bareos) x) AS only_tree,
       (SELECT count(*) FROM ls_flat) AS ls_rows,
       (SELECT count(*) FROM (SELECT path, name, jobid, fileindex FROM ls_flat WHERE kind <> 'directory'
                              EXCEPT ALL SELECT * FROM bareos WHERE name <> '') x) AS ls_files_not_in_bareos,
       (SELECT count(*) FROM (SELECT * FROM bareos WHERE name <> ''
                              EXCEPT ALL SELECT path, name, jobid, fileindex FROM ls_flat WHERE kind <> 'directory') x) AS bareos_files_not_in_ls;
