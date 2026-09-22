-- ~3.5k ALTER/CREATE statements — so it is produced, not handwritten, and this
-- file is how it is regenerated if earlier migrations change the catalog.
--
--   docker exec -i crowdrelay-postgres-1 psql -U crowdrelay -d <db> -At -f - < scripts/gen_rename_viryaos.sql > body.sql
--
-- Ordering is load-bearing: constraints and triggers address their table by
-- its old name, so they run before the table renames; RENAME CONSTRAINT carries
-- its backing index (constraint-owned indexes are excluded from the index
-- section); function renames keep the OID so trigger bindings survive, then a
-- CREATE OR REPLACE rewrites bodies that still name `viryaos_*`; compat views
-- come last, after every old name is free to be claimed again.

-- Generates migrations/0346_rename_viryaos.sql body from the live catalog.
-- Run: psql -At -f gen.sql (objects ordered so references resolve within the tx)
\pset footer off
SELECT '-- constraints (before table rename: ALTER references old table name)';
SELECT format('ALTER TABLE %I RENAME CONSTRAINT %I TO %I;',
       c.relname, con.conname, substring(con.conname from 9))
FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid
WHERE con.conname ~ '^viryaos_' AND con.contypid = 0
ORDER BY 1;
SELECT '-- domain constraints';
SELECT format('ALTER DOMAIN %I RENAME CONSTRAINT %I TO %I;',
       t.typname, con.conname, substring(con.conname from 9))
FROM pg_constraint con JOIN pg_type t ON t.oid = con.contypid
WHERE con.conname ~ '^viryaos_' AND con.contypid <> 0;
SELECT '-- triggers';
SELECT format('ALTER TRIGGER %I ON %I RENAME TO %I;',
       tg.tgname, c.relname, substring(tg.tgname from 9))
FROM pg_trigger tg JOIN pg_class c ON c.oid = tg.tgrelid
WHERE tg.tgname ~ '^viryaos_' AND NOT tg.tgisinternal
ORDER BY 1;
SELECT '-- tables / partitioned tables';
SELECT format('ALTER TABLE %I RENAME TO %I;', relname, substring(relname from 9))
FROM pg_class WHERE relkind IN ('r','p') AND relname ~ '^viryaos_' ORDER BY relname;
SELECT '-- sequences';
SELECT format('ALTER SEQUENCE %I RENAME TO %I;', relname, substring(relname from 9))
FROM pg_class WHERE relkind = 'S' AND relname ~ '^viryaos_' ORDER BY relname;
SELECT '-- indexes';
SELECT format('ALTER INDEX %I RENAME TO %I;', relname, substring(relname from 9))
FROM pg_class WHERE relkind = 'i' AND relname ~ '^viryaos_'
  AND NOT EXISTS (SELECT 1 FROM pg_constraint con WHERE con.conindid = pg_class.oid)
ORDER BY relname;
SELECT '-- existing views';
SELECT format('ALTER VIEW %I RENAME TO %I;', relname, substring(relname from 9))
FROM pg_class WHERE relkind IN ('v','m') AND relname ~ '^viryaos_' ORDER BY relname;
SELECT '-- standalone types (enum/composite/domain; rowtypes follow their table)';
SELECT format('ALTER TYPE %I RENAME TO %I;', typname, substring(typname from 9))
FROM pg_type
WHERE typname ~ '^viryaos_' AND typrelid = 0 AND typtype <> 'b' AND typelem = 0
ORDER BY typname;
SELECT '-- function renames (OID preserved: triggers and grants stay bound)';
SELECT format('ALTER FUNCTION %I(%s) RENAME TO %I;',
       p.proname, pg_get_function_identity_arguments(p.oid), substring(p.proname from 9))
FROM pg_proc p WHERE p.prokind <> 'a' AND p.proname ~ '^viryaos_' ORDER BY p.proname;
SELECT '-- function bodies (RENAME does not rewrite stored SQL text)';
SELECT replace(pg_get_functiondef(p.oid), 'viryaos_', '') || ';'
FROM pg_proc p
WHERE p.prokind <> 'a' AND pg_get_functiondef(p.oid) ~ 'viryaos_'
ORDER BY p.proname;
SELECT '-- compatibility views: old names resolve for one release window';
SELECT format('CREATE VIEW %I AS SELECT * FROM %I;', relname, substring(relname from 9))
FROM pg_class WHERE relkind IN ('r','p','v','m') AND relname ~ '^viryaos_' ORDER BY relname;
