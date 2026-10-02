import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import assert from 'node:assert/strict';
const require = createRequire(path.resolve(process.env.CROWDRELAY_VALIDATION_NODE_ROOT ?? '.', 'package.json'));
const { PGlite } = require('@electric-sql/pglite');
const db = new PGlite();
const read = p => fs.readFileSync(new URL('../'+p, import.meta.url), 'utf8');
const helper = read('crates/crowdrelay-infra/src/autopilot/lifecycle_activation.rs');
const sql = name => helper.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))[1];
const observation = read('crates/crowdrelay-infra/src/autopilot/measurement/observation.rs');
const metric = observation.slice(observation.indexOf('AutopilotMeasurementKind::FanLifecycleActivation7d =>')).match(/r#"([\s\S]*?)"#/)[1];
const ws='00000000-0000-0000-0000-000000000001', foreign='00000000-0000-0000-0000-000000000002', fan='00000000-0000-0000-0000-000000000003';
const start='2026-10-03T12:00:00Z', now='2026-10-10T12:00:00Z';
await db.exec(`
CREATE TABLE fans(id uuid,workspace_id uuid,normalized_email text,status text,deleted_at timestamptz);
CREATE TABLE fan_consents(id uuid,workspace_id uuid,fan_id uuid,purpose text,granted bool,recorded_at timestamptz);
CREATE TABLE events(id uuid,workspace_id uuid,slug text,title text,status text,starts_at timestamptz);
CREATE TABLE content_sources(id uuid,workspace_id uuid,source_kind text,source_key text,title text,active bool,occurred_at timestamptz);
CREATE TABLE ticket_orders(workspace_id uuid,buyer_email text,status text,paid_at timestamptz);
CREATE TABLE merch_order_facts(workspace_id uuid,fan_id uuid,confirmed_at timestamptz);
CREATE TABLE referral_attributions(workspace_id uuid,referrer_fan_id uuid,status text,qualified_at timestamptz);
CREATE TABLE concert_checkins(workspace_id uuid,fan_id uuid,checked_in_at timestamptz);
CREATE TABLE admission_passes(workspace_id uuid,fan_id uuid,status text,redeemed_at timestamptz);
CREATE TABLE event_interests(workspace_id uuid,fan_id uuid,created_at timestamptz);
CREATE TABLE synesthesia_reward_entries(workspace_id uuid,fan_id uuid,run_id uuid);
CREATE TABLE synesthesia_runs(id uuid,workspace_id uuid,synthetic bool,completed_at timestamptz);
-- Neither of these tables appears in the deliberate helper.
CREATE TABLE fan_sessions(workspace_id uuid,fan_id uuid,created_at timestamptz);
CREATE TABLE fan_push_endpoints(workspace_id uuid,fan_id uuid,created_at timestamptz);
`);
await db.exec(read('migrations/0397_fan_engagement_funnel.sql'));
let checks=0;
const check=(a,b)=>{checks++;assert.equal(a,b)};
await db.query("INSERT INTO events VALUES($1,$2,'real-show','Real show','published','2026-10-15'),($3,$2,'cancelled','Cancelled','cancelled','2026-10-16'),($4,$2,'past','Past','published','2026-10-01')",[fan,ws,foreign,ws]);
check((await db.query(sql('EVENT_SQL'),[ws,'real-show',now])).rows[0].slug,'real-show');
check((await db.query(sql('EVENT_SQL'),[ws,'cancelled',now])).rows.length,0);
check((await db.query(sql('EVENT_SQL'),[ws,'past',now])).rows.length,0);
check((await db.query(sql('EVENT_SQL'),[foreign,null,now])).rows.length,0);
check((await db.query(sql('EVENT_SQL'),[ws,null,now])).rows[0].slug,'real-show');
await db.query("INSERT INTO content_sources VALUES($1,$2,'video','youtube:dQw4w9WgXcQ','Real video',true,'2026-10-02'),($3,$2,'video','youtube:not-a-valid-video','Invalid',true,'2026-10-03'),($4,$2,'video','youtube:AAAAAAAAAAA','Future',true,'2026-11-03')",[fan,ws,foreign,ws]);
check((await db.query(sql('VIDEO_SQL'),[ws,'dQw4w9WgXcQ',now])).rows[0].substring,'dQw4w9WgXcQ');
check((await db.query(sql('VIDEO_SQL'),[ws,'not-a-valid-video',now])).rows.length,0);
check((await db.query(sql('VIDEO_SQL'),[foreign,null,now])).rows.length,0);
check((await db.query(sql('VIDEO_SQL'),[ws,null,now])).rows[0].title,'Real video');
await db.query("INSERT INTO fans VALUES($1,$2,'fan@example.test','active',NULL)",[fan,ws]);
await db.query("INSERT INTO fan_consents VALUES($1,$2,$1,'marketing',true,'2026-10-01')",[fan,ws]);
const value=async(at=now)=>(await db.query(metric,[ws,fan,start,at])).rows[0].case;
check(await value(),0);
await db.query("INSERT INTO fan_sessions VALUES($1,$2,'2026-10-04');",[ws,fan]);
await db.query("INSERT INTO fan_push_endpoints VALUES($1,$2,'2026-10-04');",[ws,fan]);
check(await value(),0);
await db.query("INSERT INTO event_interests VALUES($1,$2,'2026-10-04')",[ws,fan]);
check(await value(),1);
await db.query("INSERT INTO fan_consents VALUES($1,$2,$3,'marketing',false,'2026-10-05')",[foreign,ws,fan]);
check(await value(),0);
await db.exec('DELETE FROM fan_consents WHERE NOT granted; DELETE FROM event_interests;');
await db.query("INSERT INTO event_interests VALUES($1,$2,'2026-10-03T11:59:59Z'),($1,$2,'2026-10-10T12:00:00Z')",[ws,fan]);
check(await value(),0);
await db.exec('DELETE FROM event_interests;');
await db.query("INSERT INTO event_interests VALUES($1,$2,'2026-10-09')",[ws,fan]);
check(await value('2026-10-08'),0); // Future records do not activate an immature read.
await db.exec('DELETE FROM event_interests;');
await db.query("INSERT INTO synesthesia_reward_entries VALUES($1,$2,$2)",[ws,fan]);
await db.query("INSERT INTO synesthesia_runs VALUES($1,$2,true,'2026-10-04')",[fan,ws]);
check(await value(),0);
await db.exec('UPDATE synesthesia_runs SET synthetic=false;');
check(await value(),1);
await db.exec('DELETE FROM synesthesia_runs;');
for (const statement of [
 "INSERT INTO concert_checkins VALUES($1,$2,'2026-10-04')",
 "INSERT INTO merch_order_facts VALUES($1,$2,'2026-10-04')",
 "INSERT INTO referral_attributions VALUES($1,$2,'qualified','2026-10-04')",
 "INSERT INTO admission_passes VALUES($1,$2,'redeemed','2026-10-04')",
]) { await db.query(statement,[ws,fan]);check(await value(),1);await db.exec('DELETE FROM concert_checkins;DELETE FROM merch_order_facts;DELETE FROM referral_attributions;DELETE FROM admission_passes;'); }
await db.query("INSERT INTO ticket_orders VALUES($1,'fan@example.test','paid','2026-10-04')",[ws]);check(await value(),1);
await db.query("INSERT INTO concert_checkins VALUES($1,$2,'2026-10-04')",[ws,fan]);check(await value(),1); // binary fan, not event count
await db.exec("UPDATE fans SET status='unsubscribed'");check(await value(),0);
await db.exec("UPDATE fans SET status='active',deleted_at=now()");check(await value(),0);
console.log(`Welcome SQL/PostgreSQL PASS: ${checks} assertions`);
await db.close();
