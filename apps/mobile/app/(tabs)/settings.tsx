import { useState, useEffect } from 'react';
import {
  View,
  Text,
  TextInput,
  Pressable,
  ScrollView,
  StyleSheet,
  Alert,
  ActivityIndicator,
} from 'react-native';
import { useAppConfig } from '../../src/config-store';
import { setToken, setBaseUrl } from '../../src/storage';
import {
  usePushTokens,
  useRegisterPushToken,
  useTickIdleScheduler,
} from '../../src/hooks/usePushTokens';

export default function SettingsScreen() {
  const { token, baseUrl, setToken: setTokenCtx, setBaseUrl: setBaseUrlCtx } = useAppConfig();

  const [urlDraft, setUrlDraft] = useState(baseUrl);
  const [tokenDraft, setTokenDraft] = useState(token ?? '');

  useEffect(() => {
    setUrlDraft(baseUrl);
  }, [baseUrl]);

  useEffect(() => {
    setTokenDraft(token ?? '');
  }, [token]);

  const { data: pushTokensData, refetch: refetchPushTokens } = usePushTokens();
  const registerMutation = useRegisterPushToken();
  const tickMutation = useTickIdleScheduler();

  async function handleSave() {
    await Promise.all([setToken(tokenDraft.trim()), setBaseUrl(urlDraft.trim())]);
    setTokenCtx(tokenDraft.trim() || null);
    setBaseUrlCtx(urlDraft.trim());
    Alert.alert('Saved', 'Settings saved successfully.');
  }

  async function handleRegisterPush() {
    try {
      await registerMutation.mutateAsync();
      await refetchPushTokens();
      Alert.alert('Registered', 'Push token registered successfully.');
    } catch (e) {
      Alert.alert('Error', e instanceof Error ? e.message : 'Failed to register push token.');
    }
  }

  async function handleTickIdle() {
    try {
      const result = await tickMutation.mutateAsync();
      Alert.alert(
        'Idle Tick',
        `Evaluated: ${result.evaluated}\nSuspended: ${result.suspended}\nErrors: ${result.errors}`,
      );
    } catch (e) {
      Alert.alert('Error', e instanceof Error ? e.message : 'Failed to tick idle scheduler.');
    }
  }

  return (
    <ScrollView style={styles.container} contentContainerStyle={styles.content}>
      <Text style={styles.sectionTitle}>Control Plane</Text>

      <Text style={styles.label}>Base URL</Text>
      <TextInput
        style={styles.input}
        value={urlDraft}
        onChangeText={setUrlDraft}
        autoCapitalize="none"
        autoCorrect={false}
        keyboardType="url"
        placeholder="http://localhost:8787"
        placeholderTextColor="#9CA3AF"
      />

      <Text style={styles.label}>Bearer Token</Text>
      <TextInput
        style={styles.input}
        value={tokenDraft}
        onChangeText={setTokenDraft}
        secureTextEntry
        autoCapitalize="none"
        autoCorrect={false}
        placeholder="Paste your token here"
        placeholderTextColor="#9CA3AF"
      />

      <Pressable style={styles.primaryBtn} onPress={() => void handleSave()}>
        <Text style={styles.primaryBtnText}>Save</Text>
      </Pressable>

      <View style={styles.divider} />

      <Text style={styles.sectionTitle}>Push Notifications</Text>

      <Pressable
        style={[styles.secondaryBtn, registerMutation.isPending && styles.btnDisabled]}
        onPress={() => void handleRegisterPush()}
        disabled={registerMutation.isPending}
      >
        {registerMutation.isPending ? (
          <ActivityIndicator color="#2563EB" />
        ) : (
          <Text style={styles.secondaryBtnText}>Register Push Token</Text>
        )}
      </Pressable>

      {(pushTokensData?.tokens ?? []).length > 0 && (
        <View style={styles.tokenList}>
          <Text style={styles.label}>Registered Tokens</Text>
          {pushTokensData?.tokens.map((t) => (
            <View key={t.id} style={styles.tokenRow}>
              <Text style={styles.tokenLabel}>{t.label ?? 'Unnamed'}</Text>
              <Text style={styles.tokenValue} numberOfLines={1}>
                {t.token.slice(0, 32)}…
              </Text>
            </View>
          ))}
        </View>
      )}

      <View style={styles.divider} />

      <Text style={styles.sectionTitle}>Admin</Text>

      <Pressable
        style={[styles.secondaryBtn, tickMutation.isPending && styles.btnDisabled]}
        onPress={() => void handleTickIdle()}
        disabled={tickMutation.isPending}
      >
        {tickMutation.isPending ? (
          <ActivityIndicator color="#2563EB" />
        ) : (
          <Text style={styles.secondaryBtnText}>Tick Idle Scheduler</Text>
        )}
      </Pressable>
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: '#F9FAFB' },
  content: { padding: 20, gap: 12 },
  sectionTitle: { fontSize: 16, fontWeight: '700', color: '#111827', marginTop: 8 },
  label: { fontSize: 13, fontWeight: '600', color: '#374151' },
  input: {
    backgroundColor: '#FFFFFF',
    borderWidth: 1,
    borderColor: '#D1D5DB',
    borderRadius: 10,
    paddingHorizontal: 14,
    paddingVertical: 12,
    fontSize: 15,
    color: '#111827',
  },
  primaryBtn: {
    backgroundColor: '#2563EB',
    borderRadius: 10,
    paddingVertical: 14,
    alignItems: 'center',
    marginTop: 4,
  },
  primaryBtnText: { color: '#FFFFFF', fontWeight: '600', fontSize: 16 },
  secondaryBtn: {
    backgroundColor: '#FFFFFF',
    borderWidth: 1,
    borderColor: '#2563EB',
    borderRadius: 10,
    paddingVertical: 13,
    alignItems: 'center',
  },
  secondaryBtnText: { color: '#2563EB', fontWeight: '600', fontSize: 15 },
  btnDisabled: { opacity: 0.5 },
  divider: { height: 1, backgroundColor: '#E5E7EB', marginVertical: 8 },
  tokenList: { gap: 8 },
  tokenRow: {
    backgroundColor: '#FFFFFF',
    borderRadius: 8,
    padding: 12,
    gap: 2,
  },
  tokenLabel: { fontSize: 13, fontWeight: '600', color: '#374151' },
  tokenValue: { fontSize: 11, color: '#9CA3AF', fontFamily: 'monospace' },
});
