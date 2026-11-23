import { defineConnector, serve, verifyWebhook } from '../../../../sdk/typescript/index.mjs';

// Independent wire format; the approved plugin is the only format-specific code.
await serve(defineConnector({
  id: 'alternate-connector', version: '1.0.0',
  capabilities: () => ({ version: 1, optional: [] }),
  verify: verifyWebhook,
  normalize: ({ request, policy }) => {
    const { notification, destination, tenant } = JSON.parse(request.body);
    return { version: 1, source_id: policy.source_id, tenant_id: tenant ?? policy.tenant_id,
      event_id: notification.id, idempotency_key: notification.key,
      scope: policy.scope, route: destination, data: notification.attributes };
  },
}));
