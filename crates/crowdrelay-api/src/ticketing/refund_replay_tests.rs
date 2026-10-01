use super::*;

#[tokio::test]
#[ignore = "requires CROWDRELAY_TEST_DATABASE_URL and disposable PostgreSQL"]
async fn cumulative_refund_replay_preserves_monotonic_total_and_accounting()
-> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect(&std::env::var("CROWDRELAY_TEST_DATABASE_URL")?).await?;
    let workspace = WorkspaceId::new();
    let tenant = workspace.into_uuid();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO workspaces (id,slug,name) VALUES ($1,$2,'Refund replay proof')")
        .bind(tenant)
        .bind(format!("refund-{}", id.simple()))
        .execute(&pool)
        .await?;
    let event: Uuid = sqlx::query_scalar("INSERT INTO events (workspace_id,slug,title,starts_at,status) VALUES ($1,$2,'Proof',now()+interval '10 days','draft') RETURNING id")
        .bind(tenant).bind(format!("event-{}", id.simple())).fetch_one(&pool).await?;
    let admission: Uuid = sqlx::query_scalar("INSERT INTO admission_pools (workspace_id,event_id,name,slug,capacity) VALUES ($1,$2,'Proof','proof',10) RETURNING id")
        .bind(tenant).bind(event).fetch_one(&pool).await?;
    let sale: Uuid = sqlx::query_scalar("INSERT INTO ticket_sales (workspace_id,event_id,admission_pool_id,capacity,sales_open_at,sales_close_at) VALUES ($1,$2,$3,10,now()-interval '1 day',now()+interval '5 days') RETURNING id")
        .bind(tenant).bind(event).bind(admission).fetch_one(&pool).await?;
    let payment = format!("pi_{}", id.simple());
    let order: Uuid = sqlx::query_scalar("INSERT INTO ticket_orders (workspace_id,ticket_sale_id,public_reference,status,buyer_email,currency,amount_gross_minor,amount_net_minor,amount_vat_minor,vat_rate_basis_points,reservation_key,request_hash,checkout_token_hash,expires_at,paid_at,stripe_payment_intent_id) VALUES ($1,$2,'VRY-ORD-ABCDEF0123456789','paid','refund@example.test','PLN',10000,10000,0,0,'proof',digest('proof','sha256'),digest('checkout','sha256'),now()+interval '1 day',now(),$3) RETURNING id")
        .bind(tenant).bind(sale).bind(&payment).fetch_one(&pool).await?;
    let state = TicketingState::new(
        workspace,
        pool.clone(),
        Duration::from_secs(5),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );
    let request = |total: i64, suffix: &str, delta: i64| StripeTicketEventRequest {
        stripe_event_id: format!("evt_{}{suffix}", id.simple()),
        event_type: "charge.refunded".into(),
        stripe_checkout_session_id: None,
        stripe_payment_intent_id: Some(payment.clone()),
        payment_status: None,
        amount_total_minor: None,
        amount_refunded_minor: Some(total),
        currency: Some("PLN".into()),
        customer_email: None,
        occurred_at: OffsetDateTime::now_utc(),
        stripe_balance_transaction_id: Some(format!("txn_{}{suffix}", id.simple())),
        stripe_fee_minor: Some(0),
        stripe_net_minor: Some(-delta),
        stripe_reporting_category: Some("refund".into()),
    };
    let first = request(3000, "first", 3000);
    stripe_event_inner(&state, &first, None)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let stale = request(1000, "stale", 1000);
    let receipt = stripe_event_inner(&state, &stale, None)
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(receipt.received);
    assert_eq!(receipt.order.amount_refunded_minor, 3000);
    let latest = request(6000, "latest", 3000);
    stripe_event_inner(&state, &latest, None)
        .await
        .map_err(|error| format!("{error:?}"))?;
    assert!(
        stripe_event_inner(&state, &stale, None)
            .await
            .map_err(|error| format!("{error:?}"))?
            .duplicate
    );
    let (count, gross): (i64, i64) = sqlx::query_as("SELECT count(*), COALESCE(sum(amount_gross_minor),0)::bigint FROM ticket_accounting_entries WHERE workspace_id=$1 AND ticket_order_id=$2")
        .bind(tenant).bind(order).fetch_one(&pool).await?;
    assert_eq!((count, gross), (2, -6000));
    let evidence: Vec<String> = sqlx::query_scalar("SELECT stripe_balance_transaction_id FROM ticket_accounting_entries WHERE workspace_id=$1 AND ticket_order_id=$2 ORDER BY amount_gross_minor, stripe_event_id")
        .bind(tenant).bind(order).fetch_all(&pool).await?;
    assert!(evidence.contains(first.stripe_balance_transaction_id.as_ref().unwrap()));
    assert!(evidence.contains(latest.stripe_balance_transaction_id.as_ref().unwrap()));
    let oversized = request(10001, "oversized", 4001);
    assert!(matches!(
        stripe_event_inner(&state, &oversized, None).await,
        Err(TicketingError::Conflict)
    ));
    let full = request(10000, "full", 4000);
    assert_eq!(
        stripe_event_inner(&state, &full, None)
            .await
            .map_err(|error| format!("{error:?}"))?
            .order
            .status,
        "refunded"
    );
    assert_eq!(
        stripe_event_inner(&state, &request(3000, "old", 3000), None)
            .await
            .map_err(|error| format!("{error:?}"))?
            .order
            .amount_refunded_minor,
        10000
    );
    let mut conflicting = latest.clone();
    conflicting.amount_refunded_minor = Some(5999);
    assert!(matches!(
        stripe_event_inner(&state, &conflicting, None).await,
        Err(TicketingError::Conflict)
    ));
    Ok(())
}
