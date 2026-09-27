# SIP network conference

Enable a conference factory in the proxy configuration:

```toml
[proxy]
conference_factory_uri = "sip:conference@192.168.3.220:5060"
```

Configure the phone's network conference URI to the same address. The URI is a
service endpoint, not a registered extension. It is disabled when unset.

The implemented flow follows the factory and REFER-to-room patterns in
[RFC 4579 sections 5.4 and 5.6](https://www.rfc-editor.org/rfc/rfc4579.html),
matching the captured Yealink T31P signaling:

1. Establish A–B, then B–C as a separate consultation call.
2. B calls the conference factory. The PBX creates a room, answers with its
   unique room URI and `isfocus` Contact parameter, and attaches B's media.
3. B sends REFER on each old dialog, with the room URI in Refer-To. No
   Replaces is needed: the dialog carrying REFER identifies the existing call.
4. The PBX validates the dialog and attaches its opposite leg (A or C) directly
   to the room. It does not send a SIP INVITE to itself or create extra sessions.
   Final successful NOTIFY follows successful attachment.
5. B sends BYE on its old dialogs. A and C retain their original dialogs and
   sessions, including C when B was the caller of the consultation call.
6. An ordinary participant's BYE removes that member. The creator B leaving
   its new conference dialog destroys the factory-created room and sends BYE
   to the remaining participants, following RFC 4579 section 5.12. B's old
   call dialogs are not the host and can end without destroying the room.
   The manager sends ConferenceEnded to each remaining owning session, which
   queues SIP BYE through its normal hangup handler. Room teardown does not
   cancel participant sessions through a shared cancellation token.
   A room is also removed when empty; one participant alone does not close it.

The room URI also accepts ordinary dial-in and supports OPTIONS discovery.
Conference REFER executes in the old call's owning session through the
existing InboundRefer command. The session resolves the room, attaches its
opposite leg, and sends NOTIFY; no conference-specific command/reply is needed.
The handler is selected by the local room target, not by conference ownership. SIP authentication and exact existing-dialog
validation still apply. REFER/Replaces sent to the conference focus to pull
other calls into the room is not implemented. Conference roster subscriptions
are not implemented. The corrected flow needs another test on the physical
phone; the integration test reproduces its captured signaling.

The attended-transfer room implementation is saved separately. This version
has no transfer-specific room policy or cross-session transfer peer state.
