// Playwright globalSetup: the hub allows one bootstrap, so it happens once here, and the owner's
// session token travels to the test workers in the environment (it never reaches a log).

export default async function globalSetup() {
  const base = process.env.SM_BASE_URL;
  const reply = await fetch(`${base}/v1/auth/bootstrap`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${process.env.BOOTSTRAP_OWNER_TOKEN}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify({ org_name: 'Web Test Org', user_name: 'Web Owner' }),
  });
  if (reply.status !== 201) throw new Error(`bootstrap answered ${reply.status}`);
  process.env.SM_OWNER_TOKEN = (await reply.json()).session.token;
}
