# Concert QR operations

The QR confirms venue presence. It is not an admission ticket and it does not contain fan data.

Recommended workflow:

1. Synchronize or publish the concert in CrowdRelay.
2. Open the Virya staff panel at `/staff/qr`.
3. Use the default window of one hour before to five hours after start, adjusting only when necessary.
4. Generate and test the QR on a second phone before printing.
5. Print A4/SVG or show full-screen at the venue.
6. Watch the check-in count and revoke after the show or immediately after a leak.

Security properties: HMAC signature, event binding, bounded expiry, revocable database state, optional capacity and one check-in per fan/event. Limitation: a static printed QR can be photographed and shared while active; keep the window narrow and control where it is displayed.
