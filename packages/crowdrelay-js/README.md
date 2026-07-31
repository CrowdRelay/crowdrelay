# `@crowdrelay/client`

Dependency-free typed browser client for CrowdRelay. It always uses `credentials: "include"`, applies bounded request timeouts and supports idempotency keys for durable writes.

```ts
import { CrowdRelayClient } from "@crowdrelay/client";

const crowdrelay = new CrowdRelayClient({
  baseUrl: "https://api.example.com/v1/",
});

const events = await crowdrelay.listEvents();
await crowdrelay.registerEventInterest(events[0].slug);
```

Do not use direct external ticket URLs in the UI when attribution matters. Use `eventTicketUrl()` so CrowdRelay can measure the conversion and immediately redirect the fan.
