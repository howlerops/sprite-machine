// WEB-006: the members screen in Chromium against a real local hub. The members are real: each
// signed in through the PKCE flow against the mock GitHub (helpers.js githubSession).
import { expect, test } from '@playwright/test';
import { api, githubAccount, githubSession, invite, signedIn } from './helpers.js';

const rowOf = (page, account) => page.locator('#members-list li', { hasText: `@${account.login}` });
const open = async (page, session) => {
  await signedIn(page, session);
  await page.getByRole('link', { name: 'Members' }).click();
  await expect(page.getByRole('heading', { name: 'Members' })).toBeVisible();
};
const roleOf = async (token, account) => {
  const listed = await api('GET', '/v1/members', { token });
  return (listed.body.members.find((m) => m.github_login === account.login) || {}).role;
};

test.describe('members', () => {
  test('an admin sees everyone, invites by GitHub login, changes roles and removes members', async ({
    page,
  }) => {
    const admin = githubAccount('web-adm');
    const other = githubAccount('web-mem');
    const removable = githubAccount('web-rem');
    const adminSession = await githubSession(admin, 'admin');
    expect(adminSession.role).toBe('admin');
    await githubSession(other, 'member');
    const removableSession = await githubSession(removable, 'member');

    await open(page, adminSession);
    await expect(rowOf(page, other)).toContainText('@' + other.login);
    await expect(rowOf(page, removable)).toBeVisible();
    await expect(page.locator('#invite-form')).toBeVisible();

    // The admin's own row has no controls, and nor has the owner's (a higher role).
    await expect(rowOf(page, admin).locator('select, button')).toHaveCount(0);
    const ownerRow = page.locator('#members-list li', { hasText: 'Web Owner' });
    await expect(ownerRow).toBeVisible();
    await expect(ownerRow.locator('select, button')).toHaveCount(0);
    // No role above the admin's own is offered.
    const options = await rowOf(page, other).locator('select option').allTextContents();
    expect(options).toEqual(['viewer', 'member', 'admin']);

    // Invite by GitHub login: it is listed as open, and revoking it removes it.
    const invited = githubAccount('web-new');
    await page.locator('#invite-login').fill(`@${invited.login}`);
    await page.locator('#invite-role').selectOption('viewer');
    await page.getByRole('button', { name: 'Send invite' }).click();
    const inviteRow = page.locator('#invites-list li', { hasText: `@${invited.login}` });
    await expect(inviteRow).toContainText('invited as viewer');
    const openInvites = await api('GET', '/v1/invites', { token: adminSession.token });
    expect(openInvites.body.invites.map((i) => i.github_login)).toContain(invited.login);
    // The same login twice is the hub's 409, shown to the admin.
    await page.locator('#invite-login').fill(invited.login);
    await page.getByRole('button', { name: 'Send invite' }).click();
    await expect(page.getByRole('alert')).toContainText('already');
    await inviteRow.getByRole('button', { name: 'Revoke' }).click();
    await expect(inviteRow).toHaveCount(0);
    const after = await api('GET', '/v1/invites', { token: adminSession.token });
    expect(after.body.invites.map((i) => i.github_login)).not.toContain(invited.login);

    // Change a role: the hub has it, .
    expect(await roleOf(adminSession.token, other)).toBe('member');
    await rowOf(page, other).locator('select').selectOption('viewer');
    await expect.poll(() => roleOf(adminSession.token, other)).toBe('viewer');
    await expect(rowOf(page, other).locator('select')).toHaveValue('viewer');

    // Remove (two taps: the second confirms): gone from the list and its session ends at once.
    expect((await api('GET', '/v1/members', { token: removableSession.token })).status).toBe(200);
    const remove = rowOf(page, removable).getByRole('button', { name: /Remove/ });
    await remove.click();
    await expect(rowOf(page, removable)).toBeVisible();
    await remove.click();
    await expect(rowOf(page, removable)).toHaveCount(0);
    expect((await api('GET', '/v1/members', { token: removableSession.token })).status).toBe(401);
  });

  test('a viewer sees the list and nothing to change', async ({ page }) => {
    const viewer = githubAccount('web-view');
    const other = githubAccount('web-peer');
    const viewerSession = await githubSession(viewer, 'viewer');
    await githubSession(other, 'member');
    await invite(githubAccount('web-open').login, 'member');

    await open(page, viewerSession);
    await expect(rowOf(page, viewer)).toContainText('viewer');
    await expect(rowOf(page, other)).toContainText('member');
    await expect(page.locator('#invite-form')).toHaveCount(0);
    await expect(page.locator('#invites-list')).toHaveCount(0);
    await expect(page.locator('#members-list select, #members-list button')).toHaveCount(0);
    await expect(page.getByText('Only admins can invite or change members.')).toBeVisible();

    // The hub agrees: a viewer cannot invite, and the page never called the admin-only list.
    const refused = await api('POST', '/v1/invites', {
      token: viewerSession.token,
      body: { github_login: githubAccount('web-no').login, role: 'viewer' },
    });
    expect(refused.status).toBe(403);
  });
});
