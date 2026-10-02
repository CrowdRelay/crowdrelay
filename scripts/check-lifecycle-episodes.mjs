// Offline regression for the actual lifecycle SQL. See ops/growth/lifecycle-episode-acceptance.md.
import fs from 'node:fs'
import assert from 'node:assert/strict'
import path from 'node:path'
import {createRequire} from 'node:module'
// Install the optional validator in a temporary directory; no repository dependency change.
const require=createRequire(path.resolve(process.env.CROWDRELAY_VALIDATION_NODE_ROOT ?? '.', 'package.json'))
const {PGlite}=require('@electric-sql/pglite')
const db = new PGlite()
const root='../'
const core=fs.readFileSync(new URL(root+'crates/crowdrelay-infra/src/autopilot/decisions/core_reads.rs',import.meta.url),'utf8')
const section=core.slice(core.indexOf('async fn load_fan_lifecycle_snapshots_impl'))
const snapshot=section.match(/r#"([\s\S]*?)"#/)[1]
const guard=fs.readFileSync(new URL(root+'crates/crowdrelay-infra/src/autopilot/decisions/lifecycle_episodes.rs',import.meta.url),'utf8').match(/r#"([\s\S]*?)"#/)[1]
const ws='00000000-0000-0000-0000-000000000001',foreign='00000000-0000-0000-0000-000000000002',fan='00000000-0000-0000-0000-000000000003',other='00000000-0000-0000-0000-000000000004'
const now='2026-10-02T08:00:00Z',old='2026-10-01T08:00:00Z',future='2026-10-03T08:00:00Z'
await db.exec(`
CREATE TABLE fans(id uuid,workspace_id uuid,status text,created_at timestamptz,normalized_email text,deleted_at timestamptz);
CREATE TABLE fan_consents(id uuid,workspace_id uuid,fan_id uuid,purpose text,granted bool,recorded_at timestamptz);
CREATE TABLE synesthesia_reward_entries(workspace_id uuid,fan_id uuid,run_id uuid);
CREATE TABLE synesthesia_runs(id uuid,workspace_id uuid,synthetic bool,completed_at timestamptz);
CREATE TABLE workspaces(id uuid PRIMARY KEY);
CREATE TABLE admission_pools(id uuid,workspace_id uuid,event_id uuid, UNIQUE(workspace_id,id,event_id));
CREATE TABLE event_interests(workspace_id uuid,fan_id uuid,created_at timestamptz);
CREATE TABLE referral_attributions(workspace_id uuid,referrer_fan_id uuid,referred_fan_id uuid,status text,accepted_at timestamptz,qualified_at timestamptz);
CREATE TABLE referral_codes(workspace_id uuid,fan_id uuid,active bool);
CREATE TABLE signal_installations(workspace_id uuid,fan_id uuid);
CREATE TABLE autopilot_actions(id uuid,workspace_id uuid,decision_id uuid,subject_id uuid,action_kind text,status text,payload jsonb,idempotency_key text,created_at timestamptz,finished_at timestamptz);
CREATE TABLE autopilot_decisions(id uuid,workspace_id uuid,input_snapshot jsonb);
CREATE TABLE communication_campaign_recipients(workspace_id uuid,fan_id uuid,campaign_id uuid,snapshotted_at timestamptz);
CREATE TABLE communication_campaigns(id uuid,workspace_id uuid,status text,completed_at timestamptz,scheduled_at timestamptz);
CREATE TABLE concert_checkins(id uuid,workspace_id uuid,fan_id uuid,event_id uuid,checked_in_at timestamptz);
CREATE TABLE events(id uuid,workspace_id uuid,slug text,title text, UNIQUE(workspace_id,id));
`)
const ticketing=fs.readFileSync(new URL('../migrations/0011_ticketing.sql',import.meta.url),'utf8')
for(const name of ['ticket_sales','ticket_orders']) {
 const start=ticketing.indexOf('CREATE TABLE '+name+' (')
 await db.exec(ticketing.slice(start,ticketing.indexOf('\n);',start)+4))
}
await db.query('INSERT INTO workspaces VALUES($1),($2)',[ws,foreign])
await db.query("INSERT INTO fans VALUES($1,$2,'active','2026-09-25','fan@example.test',NULL),($3,$4,'active','2026-09-25','foreign@example.test',NULL)",[fan,ws,other,foreign])
await db.query("INSERT INTO fan_consents VALUES($1,$2,$3,'marketing',true,$4),($5,$2,$3,'marketing',false,$6)",[fan,ws,fan,old,other,future])
await db.query("INSERT INTO events VALUES($1,$2,'real-show','Real show'),($3,$2,'other-show','Other show')",[fan,ws,other])
await db.query("INSERT INTO admission_pools VALUES($1,$2,$1),($3,$2,$3)",[fan,ws,other])
await db.query("INSERT INTO ticket_sales(id,workspace_id,event_id,admission_pool_id,capacity,sales_open_at,sales_close_at) VALUES($1,$2,$1,$1,100,'2026-09-01','2026-11-01'),($3,$2,$3,$3,100,'2026-09-01','2026-11-01')",[fan,ws,other])
let sequence=0
async function order(at,event) {
 const key=String(++sequence).padStart(16,'0')
 await db.query("INSERT INTO ticket_orders(workspace_id,ticket_sale_id,public_reference,status,buyer_email,currency,amount_gross_minor,amount_net_minor,amount_vat_minor,vat_rate_basis_points,reservation_key,request_hash,checkout_token_hash,expires_at,paid_at) VALUES($1,$2,$3,'paid','fan@example.test','PLN',1080,1000,80,800,$3,decode(repeat('00',32),'hex'),decode(repeat('11',32),'hex'),now()+interval '2 days',$4)",[ws,event,'VRY-ORD-'+key,at])
}
for(let i=0;i<5;i++) await order(old,fan)
await order(future,other)
await db.query("INSERT INTO referral_attributions VALUES($1,$2,$3,'pending',$4,NULL)",[ws,fan,other,old])
await db.query("INSERT INTO concert_checkins VALUES($1,$2,$3,$1,$4),($5,$2,$3,$5,$6)",[fan,ws,fan,old,other,future])
await db.query("INSERT INTO event_interests VALUES($1,$2,$3)",[ws,fan,future])
await assert.rejects(db.query(snapshot.replace('DISTINCT ticket_sale.event_id','DISTINCT ticket_order.event_id'),[ws,now,100]),/event_id/)
let rows=(await db.query(snapshot,[ws,now,100])).rows
assert.equal(rows.length,1);assert.equal(rows[0].paid_ticket_count,1);assert.equal(rows[0].qualified_referrals,0);assert.equal(rows[0].last_qualified_referral_at,null);assert.equal(rows[0].marketing_consent,true);assert.equal(rows[0].checkin_event_slug,'real-show');assert.equal(rows[0].last_event_interest_at,null)
await order(old,other)
await db.query("UPDATE referral_attributions SET status='qualified',qualified_at=$1 WHERE workspace_id=$2",[old,ws])
await db.query("INSERT INTO referral_attributions VALUES($1,$2,$3,'qualified',$4,$5)",[ws,fan,fan,old,future])
rows=(await db.query(snapshot,[ws,now,100])).rows
assert.equal(rows[0].paid_ticket_count,2);assert.equal(rows[0].qualified_referrals,1);assert.equal(new Date(rows[0].last_qualified_referral_at).toISOString(),'2026-10-01T08:00:00.000Z')
assert.equal((await db.query(snapshot,[foreign,now,100])).rows[0].paid_ticket_count,0)
let count=12
const template='crowdrelay.fan.welcome.v1',current='action:lifecycle-episode:fan:welcome:once'
const answered=async({workspace=ws,subject=fan,tpl=template,key=current,episode='once',since=null,tickets=null,slug=null}={})=>(await db.query(guard,[workspace,subject,tpl,key,episode,since,tickets,slug])).rows[0].exists
assert.equal(await answered(),false);count++
async function seed({state='succeeded',template:tpl=template,key='legacy',metadata={},workspace=ws,created=old,show=null}={}) {
 await db.exec('DELETE FROM autopilot_actions; DELETE FROM autopilot_decisions;')
 await db.query('INSERT INTO autopilot_decisions VALUES($1,$2,$3)',[fan,workspace,JSON.stringify(metadata)])
 await db.query("INSERT INTO autopilot_actions VALUES($1,$2,$1,$3,'fan.lifecycle.message.request',$4,$5,$6,$7,$7)",[fan,workspace,fan,state,JSON.stringify({template_key:tpl,show}),key,created])
}
for(const state of ['awaiting_approval','queued','processing','succeeded','failed','cancelled','unknown']) {await seed({state});assert.equal(await answered(),true);count++}
assert.equal(await answered({workspace:foreign}),false);assert.equal(await answered({subject:other}),false);assert.equal(await answered({tpl:'crowdrelay.fan.signal_install_ask.v1'}),false);count+=3
await seed({metadata:{paid_ticket_count:5},template:'returning'})
assert.equal(await answered({tpl:'returning',tickets:5,episode:'shows-5'}),true);assert.equal(await answered({tpl:'returning',tickets:10,episode:'shows-10'}),false);count+=2
await seed({template:'referral-thanks'})
assert.equal(await answered({tpl:'referral-thanks',since:'2026-09-30T08:00:00Z',episode:'first'}),true);assert.equal(await answered({tpl:'referral-thanks',since:now,episode:'second'}),false);count+=2
await seed({template:'show',show:{event_slug:'real-show'}})
assert.equal(await answered({tpl:'show',since:old,slug:'real-show',episode:'first'}),true);assert.equal(await answered({tpl:'show',since:old,slug:'other-show',episode:'other'}),false);count+=2
await seed({key:current+':lapsed:test',state:'cancelled',metadata:{lifecycle_episode:{key:'once'}}});assert.equal(await answered(),false);count++
await seed({key:'other-version',metadata:{lifecycle_episode:{key:'once'}}});assert.equal(await answered(),true);count++
await seed({key:'other-version',created:future,metadata:{lifecycle_episode:{key:'old'}}});assert.equal(await answered({episode:'new',since:old}),false);count++
console.log(`PostgreSQL/PGlite PASS: ${count} assertions against actual lifecycle snapshot and compatibility SQL`)
await db.close()
