import { useState } from 'react';
import {
  View,
  Text,
  TextInput,
  Pressable,
  ScrollView,
  StyleSheet,
  ActivityIndicator,
  Alert,
} from 'react-native';
import { useRouter } from 'expo-router';
import { useCreateRun } from '../src/hooks/useRuns';
import type { CreateRunRequest } from '../src/api';

type PermissionMode = 'default' | 'acceptEdits' | 'bypassPermissions' | 'plan';
type Harness = 'claude' | 'codex' | 'pi' | 'mock';

const PERMISSION_MODES: PermissionMode[] = ['default', 'acceptEdits', 'bypassPermissions', 'plan'];
const HARNESSES: Harness[] = ['claude', 'codex', 'pi', 'mock'];

export default function NewRunScreen() {
  const router = useRouter();
  const createRun = useCreateRun();

  const [prompt, setPrompt] = useState('');
  const [workdir, setWorkdir] = useState('');
  const [permissionMode, setPermissionMode] = useState<PermissionMode>('default');
  const [harness, setHarness] = useState<Harness>('claude');

  async function handleSubmit() {
    const trimmed = prompt.trim();
    if (!trimmed) {
      Alert.alert('Prompt required', 'Please enter a prompt for the run.');
      return;
    }

    const body: CreateRunRequest = {
      prompt: trimmed,
      ...(harness !== 'claude' ? { harness } : {}),
      ...(workdir.trim() ? { workdir: workdir.trim() } : {}),
      ...(permissionMode !== 'default' ? { permissionMode } : {}),
    };

    try {
      const run = await createRun.mutateAsync(body);
      router.back();
      router.push(`/run/${run.id}`);
    } catch (e) {
      Alert.alert('Error', e instanceof Error ? e.message : 'Failed to create run.');
    }
  }

  return (
    <ScrollView
      style={styles.container}
      contentContainerStyle={styles.content}
      keyboardShouldPersistTaps="handled"
    >
      <Text style={styles.label}>Prompt *</Text>
      <TextInput
        style={[styles.input, styles.promptInput]}
        value={prompt}
        onChangeText={setPrompt}
        multiline
        autoFocus
        placeholder="What should the agent do?"
        placeholderTextColor="#9CA3AF"
        textAlignVertical="top"
      />

      <Text style={styles.label}>Harness</Text>
      <View style={styles.modeRow}>
        {HARNESSES.map((h) => (
          <Pressable
            key={h}
            style={[styles.modeBtn, harness === h && styles.modeBtnActive]}
            onPress={() => setHarness(h)}
          >
            <Text style={[styles.modeBtnText, harness === h && styles.modeBtnTextActive]}>{h}</Text>
          </Pressable>
        ))}
      </View>

      <Text style={styles.label}>Working Directory (optional)</Text>
      <TextInput
        style={styles.input}
        value={workdir}
        onChangeText={setWorkdir}
        autoCapitalize="none"
        autoCorrect={false}
        placeholder="/path/to/project"
        placeholderTextColor="#9CA3AF"
      />

      <Text style={styles.label}>Permission Mode</Text>
      <View style={styles.modeRow}>
        {PERMISSION_MODES.map((mode) => (
          <Pressable
            key={mode}
            style={[styles.modeBtn, permissionMode === mode && styles.modeBtnActive]}
            onPress={() => setPermissionMode(mode)}
          >
            <Text style={[styles.modeBtnText, permissionMode === mode && styles.modeBtnTextActive]}>
              {mode}
            </Text>
          </Pressable>
        ))}
      </View>

      <Pressable
        style={[styles.submitBtn, createRun.isPending && styles.btnDisabled]}
        onPress={() => void handleSubmit()}
        disabled={createRun.isPending}
      >
        {createRun.isPending ? (
          <ActivityIndicator color="#FFFFFF" />
        ) : (
          <Text style={styles.submitBtnText}>Start Run</Text>
        )}
      </Pressable>
    </ScrollView>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: '#F9FAFB' },
  content: { padding: 20, gap: 14 },
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
  promptInput: { minHeight: 120 },
  modeRow: { flexDirection: 'row', flexWrap: 'wrap', gap: 8 },
  modeBtn: {
    paddingHorizontal: 12,
    paddingVertical: 8,
    borderRadius: 8,
    borderWidth: 1,
    borderColor: '#D1D5DB',
    backgroundColor: '#FFFFFF',
  },
  modeBtnActive: { backgroundColor: '#2563EB', borderColor: '#2563EB' },
  modeBtnText: { fontSize: 13, color: '#374151' },
  modeBtnTextActive: { color: '#FFFFFF', fontWeight: '600' },
  submitBtn: {
    backgroundColor: '#2563EB',
    borderRadius: 10,
    paddingVertical: 15,
    alignItems: 'center',
    marginTop: 8,
  },
  submitBtnText: { color: '#FFFFFF', fontWeight: '700', fontSize: 16 },
  btnDisabled: { opacity: 0.5 },
});
