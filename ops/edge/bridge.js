'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { TextDecoder } = require('node:util');
const { DatabaseSync } = require('node:sqlite');

process.umask(0o077);
const VERSION = '10.3.0';
const PORT = integerEnv('BRIDGE_PORT', 8080, 1, 65535);
const HOST = process.env.BRIDGE_HOST || '0.0.0.0';
const CONFIG_PATH = process.env.BRIDGE_CONFIG_PATH || '/opt/bridge/config.json';
const ROUTES_PATH = process.env.BRIDGE_ROUTES_PATH || '/opt/bridge/routes.json';
const WEBHOOK_SECRET_PATH = process.env.BRIDGE_WEBHOOK_SECRET_PATH || '/run/secrets/crowdrelay_webhook_secret';
const COMMERCE_KEY_PATH = process.env.BRIDGE_COMMERCE_KEY_PATH || '/run/secrets/crowdrelay_commerce_api_key';
const MAILER_TOKEN_PATH = process.env.BRIDGE_MAILER_TOKEN_PATH || '/run/secrets/crowdrelay_mailer_token';
const TICKET_MAILER_TOKEN_PATH = process.env.BRIDGE_TICKET_MAILER_TOKEN_PATH || '/run/secrets/virya_ticket_mailer_token';
const INTERNAL_TOKEN_PATH = process.env.BRIDGE_INTERNAL_TOKEN_PATH || '/run/secrets/bridge_internal_token';
const DB_PATH = process.env.BRIDGE_DB_PATH || '/var/lib/bridge/claims.sqlite';
const MAX_BODY = integerEnv('BRIDGE_MAX_BODY_BYTES', 2 * 1024 * 1024, 1024, 8 * 1024 * 1024);
const MAX_CLOCK_SKEW_SECONDS = integerEnv('BRIDGE_MAX_CLOCK_SKEW_SECONDS', 300, 30, 3600);
const LEASE_SECONDS = integerEnv('BRIDGE_LEASE_SECONDS', 900, 60, 7200);
const COMMITTED_RETENTION_SECONDS = integerEnv('BRIDGE_COMMITTED_RETENTION_SECONDS', 180 * 86400, 86400, 365 * 86400);
const utf8 = new TextDecoder('utf-8', { fatal: true });

function integerEnv(name, fallback, min, max) {
  const parsed = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(parsed) || parsed < min || parsed > max) throw new Error(`${name} is outside ${min}..${max}`);
  return parsed;
}
function mustRead(file, label, minimum) {
  const value = fs.readFileSync(file);
  if (value.length < minimum) throw new Error(`${label} is missing or too short`);
  return value;
}
function loadJson(file, label) {
  const value = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error(`${label} must be an object`);
  return value;
}
function exactHttpsUrl(value, label) {
  const url = new URL(String(value || '').trim());
  const loopback = ['localhost', '127.0.0.1', '::1'].includes(url.hostname);
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback)) throw new Error(`${label} must use HTTPS outside loopback`);
  if (url.username || url.password || url.hash) throw new Error(`${label} contains forbidden URL components`);
  return url.toString();
}
function auditUrlFromBase(value) {
  const url = new URL(String(value || '').trim());
  const loopback = ['localhost', '127.0.0.1', '::1'].includes(url.hostname);
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback)) throw new Error('CrowdRelay API URL must use HTTPS outside loopback');
  url.username = ''; url.password = ''; url.search = ''; url.hash = '';
  const segments = url.pathname.split('/').filter(Boolean);
  const v1Index = segments.lastIndexOf('v1');
  const prefix = v1Index >= 0 ? segments.slice(0, v1Index + 1) : [...segments, 'v1'];
  url.pathname = `/${[...prefix, 'internal', 'proofs', 'audit-batches'].join('/')}`;
  return url.toString();
}
function sha256Buffer(value) { return crypto.createHash('sha256').update(value).digest('hex'); }
function safeEqualBuffer(left, right) {
  return Buffer.isBuffer(left) && Buffer.isBuffer(right) && left.length === right.length && crypto.timingSafeEqual(left, right);
}
function safeEqualHex(left, right) {
  if (!/^[a-f0-9]{64}$/i.test(left) || !/^[a-f0-9]{64}$/i.test(right)) return false;
  return safeEqualBuffer(Buffer.from(left, 'hex'), Buffer.from(right, 'hex'));
}
function nowSeconds() { return Math.floor(Date.now() / 1000); }
function writeJson(res, status, body) {
  const raw = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    'Content-Type':'application/json; charset=utf-8', 'Content-Length':raw.length,
    'Cache-Control':'no-store', 'X-Content-Type-Options':'nosniff', 'Referrer-Policy':'no-referrer',
  });
  res.end(raw);
}
function errorWithStatus(message, status, details) {
  const error = new Error(message); error.status = status; if (details) error.details = details; return error;
}
function rejection(status, error) { return { _kind:'rejected', _status:status, _error:error }; }
async function readBuffer(req) {
  let total=0; const chunks=[];
  for await (const chunk of req) {
    total += chunk.length;
    if (total > MAX_BODY) throw errorWithStatus('request_too_large', 413);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks, total);
}
async function readJson(req) {
  const raw = await readBuffer(req);
  if (!raw.length) return {};
  let text;
  try { text = utf8.decode(raw); } catch { throw errorWithStatus('invalid_utf8', 400); }
  try { return JSON.parse(text); } catch { throw errorWithStatus('invalid_json', 400); }
}

const webhookSecret = mustRead(WEBHOOK_SECRET_PATH, 'webhook secret', 32);
const commerceKey = mustRead(COMMERCE_KEY_PATH, 'commerce API key', 16).toString('utf8');
const mailerToken = mustRead(MAILER_TOKEN_PATH, 'CrowdRelay mailer token', 16).toString('utf8');
const ticketMailerToken = mustRead(TICKET_MAILER_TOKEN_PATH, 'ticket mailer token', 16).toString('utf8');
const internalToken = mustRead(INTERNAL_TOKEN_PATH, 'bridge internal token', 32);
const config = loadJson(CONFIG_PATH, 'bridge config');
const routes = loadJson(ROUTES_PATH, 'route map');
if (Object.keys(routes).length !== 49) throw new Error(`route map must contain exactly 49 event types, got ${Object.keys(routes).length}`);
for (const [eventType, workflowId] of Object.entries(routes)) {
  if (!/^[a-z0-9_.-]{3,160}$/.test(eventType) || !/^[A-Za-z0-9-]{8,80}$/.test(String(workflowId))) throw new Error('invalid route map entry');
}
const auditUrl = auditUrlFromBase(config.api_base_url || 'https://signal-api.virya.music');
const mailerUrl = exactHttpsUrl(config.mailer_url, 'CrowdRelay mailer URL');
const ticketMailerUrl = exactHttpsUrl(config.ticket_mailer_url || 'https://virya.music/api/ticket-mail', 'ticket mailer URL');

fs.mkdirSync(path.dirname(DB_PATH), { recursive:true, mode:0o700 });
const db = new DatabaseSync(DB_PATH);
db.exec(`
  PRAGMA journal_mode=WAL;
  PRAGMA synchronous=FULL;
  PRAGMA foreign_keys=ON;
  PRAGMA busy_timeout=10000;
  CREATE TABLE IF NOT EXISTS event_claims (
    claim_key TEXT PRIMARY KEY CHECK(length(claim_key)=64),
    event_id_hash TEXT NOT NULL CHECK(length(event_id_hash)=64),
    status TEXT NOT NULL CHECK(status IN ('inflight','committed')),
    claim_token TEXT,
    committed_token_hash TEXT,
    claimed_at INTEGER NOT NULL,
    committed_at INTEGER,
    updated_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS event_claims_cleanup_idx ON event_claims(status,updated_at);
`);
const columns = new Set(db.prepare('PRAGMA table_info(event_claims)').all().map((row) => row.name));
if (!columns.has('committed_token_hash')) db.exec('ALTER TABLE event_claims ADD COLUMN committed_token_hash TEXT');
const quick = db.prepare('PRAGMA quick_check').get();
if (!quick || quick.quick_check !== 'ok') throw new Error('SQLite quick_check failed at startup');

const getClaim = db.prepare('SELECT claim_key,event_id_hash,status,claim_token,committed_token_hash,claimed_at,committed_at,updated_at FROM event_claims WHERE claim_key=?');
const insertClaim = db.prepare("INSERT INTO event_claims(claim_key,event_id_hash,status,claim_token,committed_token_hash,claimed_at,committed_at,updated_at) VALUES(?,?,'inflight',?,NULL,?,NULL,?)");
const reclaimClaim = db.prepare("UPDATE event_claims SET status='inflight',claim_token=?,committed_token_hash=NULL,claimed_at=?,committed_at=NULL,updated_at=? WHERE claim_key=?");
const commitClaim = db.prepare("UPDATE event_claims SET status='committed',claim_token=NULL,committed_token_hash=?,committed_at=?,updated_at=? WHERE claim_key=? AND status='inflight' AND claim_token=?");
const deleteClaim = db.prepare("DELETE FROM event_claims WHERE claim_key=? AND status='inflight' AND claim_token=?");
const cleanupCommitted = db.prepare("DELETE FROM event_claims WHERE status='committed' AND updated_at<?");
const cleanupAbandoned = db.prepare("DELETE FROM event_claims WHERE status='inflight' AND updated_at<?");

function internalAuthorized(req) {
  const supplied = Buffer.from(String(req.headers['x-virya-bridge-token'] || ''), 'utf8');
  return safeEqualBuffer(supplied, internalToken);
}
function verifyEnvelope(raw, headers) {
  if (!raw.length) return rejection(400,'missing_raw_body');
  const timestampText=String(headers['crowdrelay-timestamp']||'');
  const signatureText=String(headers['crowdrelay-signature']||'');
  const headerEventId=String(headers['crowdrelay-event-id']||'');
  const headerEventType=String(headers['crowdrelay-event-type']||'');
  const headerEventVersion=String(headers['crowdrelay-event-version']||'');
  if (!/^\d{10}$/.test(timestampText)) return rejection(400,'invalid_timestamp');
  if (Math.abs(nowSeconds()-Number(timestampText)) > MAX_CLOCK_SKEW_SECONDS) return rejection(401,'stale_timestamp');
  const signatureMatch=/^v1=([a-f0-9]{64})$/i.exec(signatureText);
  if (!signatureMatch) return rejection(401,'invalid_signature_format');
  const expected=crypto.createHmac('sha256',webhookSecret).update(timestampText,'utf8').update('.','utf8').update(raw).digest('hex');
  if (!safeEqualHex(signatureMatch[1],expected)) return rejection(401,'signature_mismatch');
  let text;
  try { text=utf8.decode(raw); } catch { return rejection(400,'invalid_utf8'); }
  let envelope;
  try { envelope=JSON.parse(text); } catch { return rejection(400,'invalid_json'); }
  if (!envelope || typeof envelope!=='object' || Array.isArray(envelope)) return rejection(400,'invalid_envelope');
  if (!/^evt_[a-f0-9]{32}$/i.test(String(envelope.id||''))) return rejection(400,'invalid_event_id');
  if (!/^[a-z0-9_.-]{3,160}$/.test(String(envelope.type||''))) return rejection(400,'invalid_event_type');
  if (!Number.isInteger(envelope.version) || envelope.data===undefined || !envelope.workspace_id || !envelope.occurred_at) return rejection(400,'missing_envelope_fields');
  if (headerEventId!==String(envelope.id)) return rejection(400,'event_id_header_mismatch');
  if (headerEventType!==String(envelope.type)) return rejection(400,'event_type_header_mismatch');
  if (headerEventVersion!==String(envelope.version)) return rejection(400,'event_version_header_mismatch');
  if (envelope.version!==1) return rejection(422,'unsupported_event_version');
  const target=routes[String(envelope.type)];
  if (!target) return rejection(422,'unsupported_event_type');
  return { envelope,target };
}
function verifyAndClaim(raw,headers) {
  const verified=verifyEnvelope(raw,headers);
  if (verified._kind==='rejected') return verified;
  const {envelope,target}=verified;
  const eventId=String(envelope.id); const claimKey=sha256Buffer(Buffer.from(eventId)); const eventIdHash=sha256Buffer(Buffer.from(`event:${eventId}`));
  const token=crypto.randomUUID(); const now=nowSeconds();
  cleanupCommitted.run(now-COMMITTED_RETENTION_SECONDS);
  cleanupAbandoned.run(now-Math.max(LEASE_SECONDS*8,86400));
  db.exec('BEGIN IMMEDIATE');
  try {
    const row=getClaim.get(claimKey);
    if (!row) {
      insertClaim.run(claimKey,eventIdHash,token,now,now); db.exec('COMMIT');
      return {...envelope,_kind:'claim',_claim_key:claimKey,_claim_token:token,_target_workflow_id:target};
    }
    if (row.event_id_hash!==eventIdHash) { db.exec('ROLLBACK'); return rejection(409,'claim_hash_collision'); }
    if (row.status==='committed') { db.exec('COMMIT'); return {...envelope,_kind:'duplicate',_claim_key:claimKey,_target_workflow_id:target}; }
    if (now-Number(row.claimed_at)>=LEASE_SECONDS) {
      reclaimClaim.run(token,now,now,claimKey); db.exec('COMMIT');
      return {...envelope,_kind:'claim',_claim_key:claimKey,_claim_token:token,_target_workflow_id:target,_reclaimed:true};
    }
    db.exec('COMMIT');
    return {...envelope,_kind:'busy',_status:503,_error:'event_inflight',_claim_key:claimKey,_target_workflow_id:target};
  } catch(error) { try{db.exec('ROLLBACK')}catch{}; throw error; }
}
function leaseIdentity(payload) {
  const claimKey=String(payload.claim_key||''); const token=String(payload.claim_token||'');
  if (!/^[a-f0-9]{64}$/.test(claimKey) || !/^[0-9a-f-]{36}$/i.test(token)) throw errorWithStatus('invalid_claim_identity',400);
  return {claimKey,token,tokenHash:sha256Buffer(Buffer.from(token))};
}
function commit(payload) {
  const {claimKey,token,tokenHash}=leaseIdentity(payload); const now=nowSeconds();
  db.exec('BEGIN IMMEDIATE');
  try {
    const row=getClaim.get(claimKey);
    if (!row) { db.exec('ROLLBACK'); throw errorWithStatus('claim_missing',409); }
    if (row.status==='committed') {
      if (row.committed_token_hash && safeEqualHex(row.committed_token_hash,tokenHash)) { db.exec('COMMIT'); return {ok:true,committed:true,idempotent:true}; }
      db.exec('ROLLBACK'); throw errorWithStatus('claim_already_committed',409);
    }
    const result=commitClaim.run(tokenHash,now,now,claimKey,token);
    if (result.changes!==1) { db.exec('ROLLBACK'); throw errorWithStatus('claim_not_owned',409); }
    db.exec('COMMIT'); return {ok:true,committed:true,idempotent:false};
  } catch(error) { try{db.exec('ROLLBACK')}catch{}; throw error; }
}
function release(payload) {
  const {claimKey,token}=leaseIdentity(payload);
  db.exec('BEGIN IMMEDIATE');
  try {
    const row=getClaim.get(claimKey);
    if (!row) { db.exec('COMMIT'); return {ok:true,released:true,idempotent:true}; }
    if (row.status==='committed') { db.exec('ROLLBACK'); throw errorWithStatus('claim_already_committed',409); }
    const result=deleteClaim.run(claimKey,token);
    if (result.changes!==1) { db.exec('ROLLBACK'); throw errorWithStatus('claim_not_owned',409); }
    db.exec('COMMIT'); return {ok:true,released:true,idempotent:false};
  } catch(error) { try{db.exec('ROLLBACK')}catch{}; throw error; }
}
async function proxyAudit(payload) {
  const idempotencyKey=String(payload.idempotency_key||''); const correlationId=String(payload.correlation_id||''); const limit=Number(payload.limit??4096);
  if (!/^[A-Za-z0-9._:-]{1,160}$/.test(idempotencyKey)) throw errorWithStatus('invalid_idempotency_key',400);
  if (!/^[A-Za-z0-9._:-]{1,200}$/.test(correlationId)) throw errorWithStatus('invalid_correlation_id',400);
  if (!Number.isInteger(limit)||limit<1||limit>4096) throw errorWithStatus('invalid_limit',400);
  const controller=new AbortController(); const timer=setTimeout(()=>controller.abort(),30000);
  try {
    const response=await fetch(auditUrl,{method:'POST',headers:{'Content-Type':'application/json','Authorization':`Bearer ${commerceKey}`,'Idempotency-Key':idempotencyKey,'X-CrowdRelay-Correlation-ID':correlationId,'User-Agent':`Virya-CrowdRelay-n8n-Bridge/${VERSION}`},body:JSON.stringify({limit}),signal:controller.signal,redirect:'error'});
    const text=(await response.text()).slice(0,65536); let body; try{body=JSON.parse(text)}catch{body={raw:text}}
    if (!response.ok) throw errorWithStatus(`crowdrelay_audit_http_${response.status}`,response.status===429||response.status>=500?503:502,{upstream_status:response.status,upstream:body});
    return {ok:true,upstream_status:response.status,result:body};
  } finally { clearTimeout(timer); }
}


async function proxyMailerRaw(req, targetUrl, token, label) {
  const idempotencyKey=String(req.headers['idempotency-key']||'').trim();
  if (!/^[A-Za-z0-9._:-]{1,200}$/.test(idempotencyKey)) throw errorWithStatus('invalid_idempotency_key',400);
  const raw=await readBuffer(req);
  if (!raw.length) throw errorWithStatus('missing_json_body',400);
  let text;
  try { text=utf8.decode(raw); } catch { throw errorWithStatus('invalid_utf8',400); }
  try { JSON.parse(text); } catch { throw errorWithStatus('invalid_json',400); }
  const controller=new AbortController(); const timer=setTimeout(()=>controller.abort(),35000);
  try {
    const response=await fetch(targetUrl,{method:'POST',headers:{'Content-Type':'application/json','Authorization':`Bearer ${token}`,'Idempotency-Key':idempotencyKey,'User-Agent':`Virya-CrowdRelay-n8n-Bridge/${VERSION}`},body:raw,signal:controller.signal,redirect:'error'});
    const responseText=(await response.text()).slice(0,1048576); let body;
    try { body=JSON.parse(responseText); } catch { body={raw:responseText}; }
    if (!response.ok) throw errorWithStatus(`${label}_http_${response.status}`,response.status===429||response.status>=500?503:response.status,{upstream_status:response.status,upstream:body});
    return {ok:true,upstream_status:response.status,result:body};
  } finally { clearTimeout(timer); }
}
async function upstreamProbe(url, token, label) {
  const controller=new AbortController(); const timer=setTimeout(()=>controller.abort(),12000);
  const probeId=`virya-transport-probe-${Date.now()}-${crypto.randomBytes(6).toString('hex')}`;
  try {
    const response=await fetch(url,{method:'POST',headers:{'Content-Type':'application/json','Authorization':`Bearer ${token}`,'Idempotency-Key':probeId,'User-Agent':`Virya-CrowdRelay-n8n-Bridge/${VERSION}`},body:JSON.stringify({_virya_transport_probe:true,probe_id:probeId}),signal:controller.signal,redirect:'error'});
    const text=(await response.text()).slice(0,8192);
    if ([401,403,404,405,429].includes(response.status) || response.status>=500) {
      throw errorWithStatus(`${label}_preflight_http_${response.status}`,503,{upstream_status:response.status,upstream_body:text});
    }
    // A 2xx means the authenticated endpoint accepted a payload with no recipient.
    // A 4xx such as 400/409/422 means the authenticated route rejected the intentionally invalid payload.
    if (!((response.status>=200&&response.status<300)||(response.status>=400&&response.status<500))) {
      throw errorWithStatus(`${label}_preflight_unexpected_${response.status}`,503,{upstream_status:response.status});
    }
    return {status:response.status,mode:response.ok?'accepted_no_recipient':'validated_rejection'};
  } finally { clearTimeout(timer); }
}

async function upstreamHealth() {
  const [mailer,ticket]=await Promise.all([
    upstreamProbe(mailerUrl,mailerToken,'mailer'),
    upstreamProbe(ticketMailerUrl,ticketMailerToken,'ticket_mailer'),
  ]);
  return {ok:true,mailer_status:mailer.status,mailer_mode:mailer.mode,ticket_mailer_status:ticket.status,ticket_mailer_mode:ticket.mode,messages_sent:0};
}

const server=http.createServer(async(req,res)=>{
  const started=Date.now(); let status=500;
  try {
    if (req.method==='GET' && (req.url==='/health'||req.url==='/ready')) {
      const check=db.prepare('PRAGMA quick_check').get(); status=check?.quick_check==='ok'?200:503;
      return writeJson(res,status,{ok:status===200,component:'virya-crowdrelay-n8n-bridge',version:VERSION,sqlite:check?.quick_check||'unknown',routes:Object.keys(routes).length,mailer_configured:true,ticket_mailer_configured:true});
    }
    if (req.method!=='POST') {status=405; return writeJson(res,status,{ok:false,error:'method_not_allowed'});}
    if (!internalAuthorized(req)) {status=401; return writeJson(res,status,{ok:false,error:'bridge_auth_failed'});}
    if (req.url==='/verify-claim') { const raw=await readBuffer(req); status=200; return writeJson(res,status,verifyAndClaim(raw,req.headers)); }
    if (req.url==='/mailer') {status=200; return writeJson(res,status,await proxyMailerRaw(req,mailerUrl,mailerToken,'mailer'));}
    if (req.url==='/ticket-mailer') {status=200; return writeJson(res,status,await proxyMailerRaw(req,ticketMailerUrl,ticketMailerToken,'ticket_mailer'));}
    const payload=await readJson(req);
    if (req.url==='/commit') {status=200; return writeJson(res,status,commit(payload));}
    if (req.url==='/release') {status=200; return writeJson(res,status,release(payload));}
    if (req.url==='/audit-batch') {status=200; return writeJson(res,status,await proxyAudit(payload));}
    if (req.url==='/upstream-health') {status=200; return writeJson(res,status,await upstreamHealth());}
    status=404; return writeJson(res,status,{ok:false,error:'not_found'});
  } catch(error) {
    status=Number(error.status)||(error.name==='AbortError'?503:500);
    const body={ok:false,error:error.message||'internal_error'}; if(error.details)body.details=error.details;
    return writeJson(res,status,body);
  } finally {
    process.stdout.write(JSON.stringify({level:'info',message:'request',method:req.method,path:req.url,status,duration_ms:Date.now()-started})+'\n');
  }
});
server.requestTimeout=40000; server.headersTimeout=10000; server.keepAliveTimeout=5000; server.maxRequestsPerSocket=100; server.maxConnections=64;
server.listen(PORT,HOST,()=>process.stdout.write(JSON.stringify({level:'info',message:'bridge_listening',version:VERSION,host:HOST,port:PORT,audit_origin:new URL(auditUrl).origin,mailer_origin:new URL(mailerUrl).origin,ticket_mailer_origin:new URL(ticketMailerUrl).origin,routes:Object.keys(routes).length})+'\n'));
function shutdown(signal){server.close(()=>{try{db.exec('PRAGMA wal_checkpoint(TRUNCATE)')}catch{};try{db.close()}catch{};process.stdout.write(JSON.stringify({level:'info',message:'bridge_stopped',signal})+'\n');process.exit(0)});setTimeout(()=>process.exit(1),10000).unref()}
process.on('SIGTERM',()=>shutdown('SIGTERM')); process.on('SIGINT',()=>shutdown('SIGINT'));
process.on('uncaughtException',(error)=>{process.stderr.write(JSON.stringify({level:'fatal',message:'uncaught_exception',error:error.message})+'\n');process.exit(1)});
process.on('unhandledRejection',(error)=>{process.stderr.write(JSON.stringify({level:'fatal',message:'unhandled_rejection',error:String(error?.message||error)})+'\n');process.exit(1)});
