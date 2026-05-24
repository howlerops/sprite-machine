import { useState, useEffect } from 'react';
import { Platform } from 'react-native';
import { useRouter } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { ScrollView } from '@/components/ui/scroll-view';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Divider } from '@/components/ui/divider';
import { Input, InputField, InputSlot } from '@/components/ui/input';
import { Textarea, TextareaInput } from '@/components/ui/textarea';
import { Icon, EyeIcon, EyeOffIcon } from '@/components/ui/icon';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import { FormControl, FormControlLabel, FormControlLabelText } from '@/components/ui/form-control';
import { useToast, Toast, ToastTitle, ToastDescription } from '@/components/ui/toast';

import { useAppConfig } from '../../src/config-store';
import { setToken, setBaseUrl, clearEmail, type ThemeOverride } from '../../src/storage';
import { logout } from '../../src/api';
import {
  usePushTokens,
  useRegisterPushToken,
  useTickIdleScheduler,
} from '../../src/hooks/usePushTokens';
import {
  useAiAuthIntegration,
  useApplyAiAuthIntegration,
  useApplyGithubIntegration,
  useGithubIntegration,
  useSaveAiAuthIntegration,
  useSaveGithubIntegration,
} from '../../src/hooks/useIntegrations';

const THEME_OPTIONS: ThemeOverride[] = ['system', 'light', 'dark'];

export default function SettingsScreen() {
  const router = useRouter();
  const {
    token,
    baseUrl,
    email,
    themeOverride,
    setToken: setTokenCtx,
    setBaseUrl: setBaseUrlCtx,
    setEmail: setEmailCtx,
    setThemeOverride,
  } = useAppConfig();

  const toast = useToast();
  const [urlDraft, setUrlDraft] = useState(baseUrl);
  const [tokenDraft, setTokenDraft] = useState(token ?? '');
  const [showToken, setShowToken] = useState(false);
  const [signingOut, setSigningOut] = useState(false);
  const [ghToken, setGhToken] = useState('');
  const [ghUsername, setGhUsername] = useState('jbeck018');
  const [gitName, setGitName] = useState('Jacob Beck');
  const [gitEmail, setGitEmail] = useState('jacob.beck.018@gmail.com');
  const [codexAuthJson, setCodexAuthJson] = useState('');
  const [piAuthJson, setPiAuthJson] = useState('');
  const [claudeCredentialsJson, setClaudeCredentialsJson] = useState('');

  useEffect(() => {
    setUrlDraft(baseUrl);
  }, [baseUrl]);

  useEffect(() => {
    setTokenDraft(token ?? '');
  }, [token]);

  const { data: pushTokensData, refetch: refetchPushTokens } = usePushTokens();
  const { data: githubIntegration } = useGithubIntegration();
  const { data: aiAuthIntegration } = useAiAuthIntegration();
  const saveGithubMutation = useSaveGithubIntegration();
  const applyGithubMutation = useApplyGithubIntegration();
  const saveAiAuthMutation = useSaveAiAuthIntegration();
  const applyAiAuthMutation = useApplyAiAuthIntegration();
  const registerMutation = useRegisterPushToken();
  const tickMutation = useTickIdleScheduler();

  useEffect(() => {
    if (!githubIntegration) return;
    if (githubIntegration.username) setGhUsername(githubIntegration.username);
    if (githubIntegration.gitUserName) setGitName(githubIntegration.gitUserName);
    if (githubIntegration.gitUserEmail) setGitEmail(githubIntegration.gitUserEmail);
  }, [githubIntegration]);

  function showToast(opts: {
    title: string;
    description?: string;
    action?: 'success' | 'error' | 'info';
  }) {
    const id = String(Math.random());
    toast.show({
      id,
      placement: 'top',
      duration: 3000,
      render: () => (
        <Toast nativeID={`toast-${id}`} action={opts.action ?? 'info'} variant="solid">
          <ToastTitle>{opts.title}</ToastTitle>
          {opts.description ? <ToastDescription>{opts.description}</ToastDescription> : null}
        </Toast>
      ),
    });
  }

  async function handleSave() {
    await Promise.all([setToken(tokenDraft.trim()), setBaseUrl(urlDraft.trim())]);
    setTokenCtx(tokenDraft.trim() || null);
    setBaseUrlCtx(urlDraft.trim());
    showToast({ title: 'Saved', description: 'Settings saved successfully.', action: 'success' });
  }

  async function handleRegisterPush() {
    try {
      await registerMutation.mutateAsync();
      await refetchPushTokens();
      showToast({
        title: 'Registered',
        description: 'Push token registered successfully.',
        action: 'success',
      });
    } catch (e) {
      showToast({
        title: 'Error',
        description: e instanceof Error ? e.message : 'Failed to register push token.',
        action: 'error',
      });
    }
  }

  async function handleTickIdle() {
    try {
      const result = await tickMutation.mutateAsync();
      showToast({
        title: 'Idle tick',
        description: `Evaluated: ${result.evaluated}, Suspended: ${result.suspended}, Errors: ${result.errors}`,
        action: 'info',
      });
    } catch (e) {
      showToast({
        title: 'Error',
        description: e instanceof Error ? e.message : 'Failed to tick idle scheduler.',
        action: 'error',
      });
    }
  }

  async function handleSaveGithub() {
    try {
      const result = await saveGithubMutation.mutateAsync({
        ...(ghToken.trim() ? { token: ghToken.trim() } : {}),
        username: ghUsername.trim(),
        gitUserName: gitName.trim(),
        gitUserEmail: gitEmail.trim(),
      });
      setGhToken('');
      const applied = result.apply?.applied.length ?? 0;
      const errors = result.apply?.errors.length ?? 0;
      showToast({
        title: 'GitHub saved',
        description: `Applied to ${applied} sprite(s)${errors ? `, ${errors} error(s)` : ''}.`,
        action: errors ? 'info' : 'success',
      });
    } catch (e) {
      showToast({
        title: 'GitHub error',
        description: e instanceof Error ? e.message : 'Failed to save GitHub integration.',
        action: 'error',
      });
    }
  }

  async function handleApplyGithub() {
    try {
      const result = await applyGithubMutation.mutateAsync();
      showToast({
        title: 'GitHub applied',
        description: `Applied to ${result.apply?.applied.length ?? 0} sprite(s).`,
        action: 'success',
      });
    } catch (e) {
      showToast({
        title: 'GitHub error',
        description: e instanceof Error ? e.message : 'Failed to apply GitHub integration.',
        action: 'error',
      });
    }
  }

  async function handleSaveAiAuth() {
    try {
      const body = {
        ...(codexAuthJson.trim() ? { codexAuthJson: codexAuthJson.trim() } : {}),
        ...(piAuthJson.trim() ? { piAuthJson: piAuthJson.trim() } : {}),
        ...(claudeCredentialsJson.trim()
          ? { claudeCredentialsJson: claudeCredentialsJson.trim() }
          : {}),
      };
      const result = await saveAiAuthMutation.mutateAsync(body);
      setCodexAuthJson('');
      setPiAuthJson('');
      setClaudeCredentialsJson('');
      const applied = result.apply?.applied.length ?? 0;
      const errors = result.apply?.errors.length ?? 0;
      showToast({
        title: 'AI auth saved',
        description: `Applied to ${applied} sprite(s)${errors ? `, ${errors} error(s)` : ''}.`,
        action: errors ? 'info' : 'success',
      });
    } catch (e) {
      showToast({
        title: 'AI auth error',
        description: e instanceof Error ? e.message : 'Failed to save AI auth.',
        action: 'error',
      });
    }
  }

  async function handleApplyAiAuth() {
    try {
      const result = await applyAiAuthMutation.mutateAsync();
      showToast({
        title: 'AI auth applied',
        description: `Applied to ${result.apply?.applied.length ?? 0} sprite(s).`,
        action: 'success',
      });
    } catch (e) {
      showToast({
        title: 'AI auth error',
        description: e instanceof Error ? e.message : 'Failed to apply AI auth.',
        action: 'error',
      });
    }
  }

  async function handleSignOut() {
    setSigningOut(true);
    try {
      await logout(baseUrl);
    } catch {
      // Ignore logout errors — clear local state regardless.
    }
    await clearEmail();
    setEmailCtx(null);
    setSigningOut(false);
    router.replace('/login');
  }

  return (
    <ScrollView className="flex-1 bg-background">
      <VStack space="2xl" className="p-5 md:p-8 w-full max-w-2xl mx-auto">
        {/* Control Plane — Base URL (always visible) */}
        <VStack space="md">
          <Heading size="md" bold className="text-foreground">
            Control Plane
          </Heading>

          <FormControl>
            <FormControlLabel>
              <FormControlLabelText>Base URL</FormControlLabelText>
            </FormControlLabel>
            <Input>
              <InputField
                value={urlDraft}
                onChangeText={setUrlDraft}
                autoCapitalize="none"
                autoCorrect={false}
                keyboardType="url"
                placeholder="http://localhost:8787"
              />
            </Input>
          </FormControl>

          {/* Native-only: Bearer Token field */}
          {Platform.OS !== 'web' ? (
            <FormControl>
              <FormControlLabel>
                <FormControlLabelText>Bearer Token</FormControlLabelText>
              </FormControlLabel>
              <Input>
                <InputField
                  value={tokenDraft}
                  onChangeText={setTokenDraft}
                  secureTextEntry={!showToken}
                  autoCapitalize="none"
                  autoCorrect={false}
                  placeholder="Paste your token here"
                />
                <InputSlot onPress={() => setShowToken((v) => !v)} className="pr-3">
                  <Icon
                    as={showToken ? EyeOffIcon : EyeIcon}
                    size="sm"
                    className="text-muted-foreground"
                  />
                </InputSlot>
              </Input>
            </FormControl>
          ) : null}

          <Button
            variant="default"
            size="lg"
            className="self-start"
            onPress={() => void handleSave()}
          >
            <ButtonText>Save</ButtonText>
          </Button>
        </VStack>

        <Divider />

        {/* Session — Web: email + sign out. Native: nothing extra (bearer is above). */}
        {Platform.OS === 'web' ? (
          <>
            <VStack space="md">
              <Heading size="md" bold className="text-foreground">
                Session
              </Heading>
              {email ? (
                <VStack space="xs">
                  <Text size="sm" bold className="text-foreground">
                    {email}
                  </Text>
                  <Text size="xs" className="font-mono text-muted-foreground">
                    Signed in via Fly
                  </Text>
                </VStack>
              ) : null}
              <Button
                variant="outline"
                size="sm"
                className="self-start"
                isDisabled={signingOut}
                onPress={() => void handleSignOut()}
              >
                {signingOut ? <ButtonSpinner /> : null}
                <ButtonText>Sign out</ButtonText>
              </Button>
            </VStack>
            <Divider />
          </>
        ) : null}

        {/* Integrations */}
        <VStack space="md">
          <Heading size="md" bold className="text-foreground">
            Integrations
          </Heading>

          <Box className="bg-card border border-border rounded-[10px] p-4">
            <VStack space="md">
              <HStack className="items-center" space="sm">
                <VStack className="flex-1" space="xs">
                  <Text size="sm" bold className="text-foreground">
                    AI provider auth
                  </Text>
                  <Text size="xs" className="text-muted-foreground">
                    Paste local auth JSON for Codex/Pi/Claude. Values are encrypted and applied to
                    worker sprites.
                  </Text>
                </VStack>
                <Text size="2xs" bold className="uppercase text-muted-foreground">
                  {[
                    aiAuthIntegration?.codexConnected ? 'Codex' : null,
                    aiAuthIntegration?.piConnected ? 'Pi' : null,
                    aiAuthIntegration?.claudeConnected ? 'Claude' : null,
                  ]
                    .filter(Boolean)
                    .join(' / ') || 'Not connected'}
                </Text>
              </HStack>

              <Text size="xs" className="text-muted-foreground">
                Local file paths: ~/.codex/auth.json, ~/.pi/agent/auth.json, and
                ~/.claude/.credentials.json. Leave fields blank to keep existing saved values.
              </Text>

              <FormControl>
                <FormControlLabel>
                  <FormControlLabelText>Codex auth.json</FormControlLabelText>
                </FormControlLabel>
                <Textarea className="h-24">
                  <TextareaInput
                    value={codexAuthJson}
                    onChangeText={setCodexAuthJson}
                    autoCapitalize="none"
                    autoCorrect={false}
                    placeholder={
                      aiAuthIntegration?.codexConnected
                        ? 'Connected — paste new JSON to replace'
                        : '{ "tokens": ... }'
                    }
                  />
                </Textarea>
              </FormControl>

              <FormControl>
                <FormControlLabel>
                  <FormControlLabelText>Pi auth.json</FormControlLabelText>
                </FormControlLabel>
                <Textarea className="h-24">
                  <TextareaInput
                    value={piAuthJson}
                    onChangeText={setPiAuthJson}
                    autoCapitalize="none"
                    autoCorrect={false}
                    placeholder={
                      aiAuthIntegration?.piConnected
                        ? 'Connected — paste new JSON to replace'
                        : '{ "openai-codex": ... }'
                    }
                  />
                </Textarea>
              </FormControl>

              <FormControl>
                <FormControlLabel>
                  <FormControlLabelText>Claude credentials JSON</FormControlLabelText>
                </FormControlLabel>
                <Textarea className="h-24">
                  <TextareaInput
                    value={claudeCredentialsJson}
                    onChangeText={setClaudeCredentialsJson}
                    autoCapitalize="none"
                    autoCorrect={false}
                    placeholder={
                      aiAuthIntegration?.claudeConnected
                        ? 'Connected — paste new JSON to replace'
                        : '{ "claudeAiOauth": ... }'
                    }
                  />
                </Textarea>
              </FormControl>

              <HStack space="sm" className="flex-wrap">
                <Button
                  variant="default"
                  size="sm"
                  isDisabled={saveAiAuthMutation.isPending}
                  onPress={() => void handleSaveAiAuth()}
                >
                  {saveAiAuthMutation.isPending ? <ButtonSpinner /> : null}
                  <ButtonText>Save & Apply AI auth</ButtonText>
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  isDisabled={
                    applyAiAuthMutation.isPending ||
                    !(
                      aiAuthIntegration?.codexConnected ||
                      aiAuthIntegration?.piConnected ||
                      aiAuthIntegration?.claudeConnected
                    )
                  }
                  onPress={() => void handleApplyAiAuth()}
                >
                  {applyAiAuthMutation.isPending ? <ButtonSpinner /> : null}
                  <ButtonText>Apply saved auth</ButtonText>
                </Button>
              </HStack>
            </VStack>
          </Box>

          <Box className="bg-card border border-border rounded-[10px] p-4">
            <VStack space="md">
              <HStack className="items-center" space="sm">
                <VStack className="flex-1" space="xs">
                  <Text size="sm" bold className="text-foreground">
                    GitHub
                  </Text>
                  <Text size="xs" className="text-muted-foreground">
                    {githubIntegration?.connected
                      ? `Connected as ${githubIntegration.username}`
                      : 'Paste a GitHub token once; sprites pick it up automatically.'}
                  </Text>
                </VStack>
                <Text
                  size="2xs"
                  bold
                  className={
                    githubIntegration?.connected
                      ? 'uppercase text-status-success'
                      : 'uppercase text-muted-foreground'
                  }
                >
                  {githubIntegration?.connected ? 'Connected' : 'Not connected'}
                </Text>
              </HStack>

              <FormControl>
                <FormControlLabel>
                  <FormControlLabelText>GitHub token</FormControlLabelText>
                </FormControlLabel>
                <Input>
                  <InputField
                    value={ghToken}
                    onChangeText={setGhToken}
                    secureTextEntry
                    autoCapitalize="none"
                    autoCorrect={false}
                    placeholder={
                      githubIntegration?.connected
                        ? 'Leave blank to keep existing token'
                        : 'ghp_… or github_pat_…'
                    }
                  />
                </Input>
              </FormControl>

              <HStack space="sm" className="flex-wrap">
                <Box className="flex-1 min-w-[180px]">
                  <FormControl>
                    <FormControlLabel>
                      <FormControlLabelText>GitHub username</FormControlLabelText>
                    </FormControlLabel>
                    <Input>
                      <InputField
                        value={ghUsername}
                        onChangeText={setGhUsername}
                        autoCapitalize="none"
                      />
                    </Input>
                  </FormControl>
                </Box>
                <Box className="flex-1 min-w-[180px]">
                  <FormControl>
                    <FormControlLabel>
                      <FormControlLabelText>Git name</FormControlLabelText>
                    </FormControlLabel>
                    <Input>
                      <InputField value={gitName} onChangeText={setGitName} />
                    </Input>
                  </FormControl>
                </Box>
              </HStack>

              <FormControl>
                <FormControlLabel>
                  <FormControlLabelText>Git email</FormControlLabelText>
                </FormControlLabel>
                <Input>
                  <InputField
                    value={gitEmail}
                    onChangeText={setGitEmail}
                    autoCapitalize="none"
                    keyboardType="email-address"
                  />
                </Input>
              </FormControl>

              <HStack space="sm" className="flex-wrap">
                <Button
                  variant="default"
                  size="sm"
                  isDisabled={saveGithubMutation.isPending}
                  onPress={() => void handleSaveGithub()}
                >
                  {saveGithubMutation.isPending ? <ButtonSpinner /> : null}
                  <ButtonText>Save & Apply</ButtonText>
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  isDisabled={!githubIntegration?.connected || applyGithubMutation.isPending}
                  onPress={() => void handleApplyGithub()}
                >
                  {applyGithubMutation.isPending ? <ButtonSpinner /> : null}
                  <ButtonText>Apply to sprites</ButtonText>
                </Button>
              </HStack>
            </VStack>
          </Box>
        </VStack>

        <Divider />

        {/* Push Notifications */}
        <VStack space="md">
          <Heading size="md" bold className="text-foreground">
            Push Notifications
          </Heading>

          <Button
            variant="outline"
            size="default"
            isDisabled={registerMutation.isPending}
            onPress={() => void handleRegisterPush()}
            className="self-start"
          >
            {registerMutation.isPending ? <ButtonSpinner /> : null}
            <ButtonText>Register Push Token</ButtonText>
          </Button>

          {(pushTokensData?.tokens ?? []).length > 0 ? (
            <VStack space="sm">
              <Text size="xs" bold className="text-muted-foreground uppercase">
                Registered Tokens
              </Text>
              {pushTokensData?.tokens.map((t) => (
                <Box key={t.id} className="bg-card border border-border rounded-[10px] p-3">
                  <Text size="sm" bold className="text-foreground">
                    {t.label ?? 'Unnamed'}
                  </Text>
                  <Text size="xs" isTruncated className="text-muted-foreground font-mono">
                    {t.token.slice(0, 32)}…
                  </Text>
                </Box>
              ))}
            </VStack>
          ) : null}
        </VStack>

        <Divider />

        {/* Appearance */}
        <VStack space="md">
          <Heading size="md" bold className="text-foreground">
            Appearance
          </Heading>
          <Text size="xs" className="text-muted-foreground">
            Override the system theme. Changes apply immediately.
          </Text>
          <HStack space="sm" className="flex-wrap">
            {THEME_OPTIONS.map((opt) => {
              const active = themeOverride === opt;
              return (
                <Pressable
                  key={opt}
                  onPress={() => setThemeOverride(opt)}
                  className={
                    active
                      ? 'px-3 py-2 rounded-[10px] border border-primary bg-primary'
                      : 'px-3 py-2 rounded-[10px] border border-border bg-background data-[hover=true]:bg-accent'
                  }
                >
                  <Text
                    size="sm"
                    bold={active}
                    className={active ? 'text-primary-foreground' : 'text-foreground'}
                  >
                    {opt}
                  </Text>
                </Pressable>
              );
            })}
          </HStack>
        </VStack>

        <Divider />

        {/* Admin */}
        <VStack space="md">
          <Heading size="md" bold className="text-foreground">
            Admin
          </Heading>
          <Button
            variant="outline"
            size="default"
            isDisabled={tickMutation.isPending}
            onPress={() => void handleTickIdle()}
            className="self-start"
          >
            {tickMutation.isPending ? <ButtonSpinner /> : null}
            <ButtonText>Tick Idle Scheduler</ButtonText>
          </Button>
        </VStack>
      </VStack>
    </ScrollView>
  );
}
