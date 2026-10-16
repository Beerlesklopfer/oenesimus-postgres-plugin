-- Benchmark catalog "pc-1m": the size of a PC backup, about a million files.
--
-- Run in an empty database created like Bareos creates its catalog, after
-- loading the Bareos DDL (see run.sh):
--   CREATE DATABASE ovfs_bench ENCODING 'SQL_ASCII' LC_COLLATE 'C' LC_CTYPE 'C' TEMPLATE template0;
--
-- Content (deterministic, no random values):
--   client fd1, FileSet LinuxAll, one enabled volume Full-0001
--   paths   /, /data/, /data/d1/ .. /data/d500/, /data/dN/s1/ .. /data/dN/s100/
--           = 50,502 directories (500 x 100 leaf directories)
--   job 1   Full, 2026-10-09 00:00 UTC
--           one row per directory (50,502) + 20 files per leaf (file-1.dat .. file-20.dat)
--           = 1,050,502 rows
--   job 2   Incremental, 2026-10-10 00:00 UTC
--           4 new files per leaf (inc-1.dat .. inc-4.dat)          200,000 rows
--           file-1.dat changed in every leaf                        50,000 rows
--           file-2.dat deleted in the leaves of d1 .. d200 (FileIndex 0)  20,000 rows
--           = 270,000 rows
--   catalog 1,320,502 File rows; the chain 1+2 has 1,230,502 live entries
--
-- LStat values: directories mode 040755, files mode 0100644, size 4711,
-- mtime 1700000000 (see ovfs-core lstat tests).

\set ON_ERROR_STOP on

INSERT INTO client (clientid, name, uname) VALUES (1, 'fd1', 'bench');
INSERT INTO fileset (filesetid, fileset, md5, createtime) VALUES (1, 'LinuxAll', 'bench', '2026-10-01');
INSERT INTO media (mediaid, volumename, mediatype, volstatus, enabled) VALUES (1, 'Full-0001', 'File', 'Full', 1);

INSERT INTO job (jobid, job, name, type, level, jobstatus, clientid, filesetid, starttime, jobtdate) VALUES
    (1, 'backup-fd1.2026-10-09', 'backup-fd1', 'B', 'F', 'T', 1, 1, '2026-10-09 00:00:00', 1791504000),
    (2, 'backup-fd1.2026-10-10', 'backup-fd1', 'B', 'I', 'T', 1, 1, '2026-10-10 00:00:00', 1791590400);
INSERT INTO jobmedia (jobid, mediaid, firstindex, lastindex) VALUES (1, 1, 1, 2000000), (2, 1, 1, 2000000);

INSERT INTO path (pathid, path) VALUES (1, '/'), (2, '/data/');
INSERT INTO path (pathid, path) SELECT 2 + i, format('/data/d%s/', i) FROM generate_series(1, 500) i;
INSERT INTO path (pathid, path)
    SELECT 502 + (i - 1) * 100 + j, format('/data/d%s/s%s/', i, j)
    FROM generate_series(1, 500) i, generate_series(1, 100) j;

-- job 1: directory rows, then 20 files per leaf
INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name)
    SELECT pathid, 1, pathid, 'A A EHt B A A A A BAA A A BlU/EA A A A A', '', '' FROM path;
INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name)
    SELECT 100000 + (p.pathid - 503) * 20 + k, 1, p.pathid,
           'A A IGk B A A A BJn BAA A A BlU/EA A A A A', '', format('file-%s.dat', k)
    FROM path p, generate_series(1, 20) k WHERE p.pathid > 502;

-- job 2: new files, changed file-1.dat, deleted file-2.dat in d1 .. d200
INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name)
    SELECT (p.pathid - 503) * 4 + k, 2, p.pathid,
           'A A IGk B A A A BJn BAA A A BlU/EA A A A A', '', format('inc-%s.dat', k)
    FROM path p, generate_series(1, 4) k WHERE p.pathid > 502;
INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name)
    SELECT 200000 + (p.pathid - 502), 2, p.pathid,
           'A A IGk B A A A BJn BAA A A BlU/EA A A A A', '', 'file-1.dat'
    FROM path p WHERE p.pathid > 502;
INSERT INTO file (fileindex, jobid, pathid, lstat, md5, name)
    SELECT 0, 2, p.pathid, 'A A IGk B A A A BJn BAA A A BlU/EA A A A A', '', 'file-2.dat'
    FROM path p WHERE p.pathid > 502 AND p.pathid <= 502 + 200 * 100;

ANALYZE;

SELECT jobid, count(*) AS rows, count(*) FILTER (WHERE fileindex = 0) AS deletions
FROM file GROUP BY jobid ORDER BY jobid;
