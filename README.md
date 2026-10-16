# oenesimus-postgres-plugin

`ovfs`, a PostgreSQL extension that provides the directory trees of Bareos
backup jobs for restores: a JSON tree of a chosen depth and directory listings
for loading a tree model on demand. It reads the Bareos catalog unchanged
(schema 2250) and is built to run on small machines (1–2 GB RAM).

```sql
CREATE EXTENSION ovfs;
SELECT ovfs.tree_latest('pc-anna', '/', 2);
SELECT * FROM ovfs.ls(ovfs.jobids_latest('pc-anna'), '/home/anna/');
```

Documentation: [Wiki](https://github.com/Beerlesklopfer/oenesimus-postgres-plugin/wiki)

| Path | Content |
|---|---|
| `ovfs/` | Cargo workspace: `ovfs-core` (catalog logic) and `ovfs` (pgrx extension) |
| `Containerfile`, `compose.yaml` | development container with PostgreSQL 19, pgrx and OpenCode |
| `config/`, `scripts/` | container configuration and start script |

Targets PostgreSQL 19 and Bareos 25.1.2-pre.
