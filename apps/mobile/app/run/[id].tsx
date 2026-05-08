import { useState, useRef, useCallback } from 'react';
import {
  View,
  Text,
  TextInput,
  Pressable,
  FlatList,
  StyleSheet,
  Alert,
  ActivityIndicator,
  ScrollView,
} from 'react-native';
import { useLocalSearchParams, useRouter } from 'expo-router';
import {
  useRun,
  useSendMessage,
  useCancelRun,
  useResolvePermission,
} from '../../src/hooks/useRuns';
import { useSuspendSprite, useResumeSprite } from '../../src/hooks/useSprites';
import { useRunEventStream } from '../../src/sse';
import { statusColor, formatCost } from '../../src/utils';
import type { RunEventRow } from '../../src/api';
import type { RunEvent } from '@sprite-machine/shared';

function EventCard({
  row,
  runId,
  onResolve,
}: {
  row: RunEventRow;
  runId: string;
  onResolve: (requestId: string, decision: 'allow' | 'deny') => void;
}) {
  const { event } = row;

  switch (event.type) {
    case 'assistant_text':
      return (
        <View style={[styles.eventCard, styles.assistantCard]}>
          <Text style={styles.eventRole}>Assistant</Text>
          <Text style={styles.eventText}>{event.text}</Text>
        </View>
      );

    case 'user_message':
      return (
        <View style={[styles.eventCard, styles.userCard]}>
          <Text style={styles.eventRole}>You</Text>
          <Text style={styles.eventText}>{event.text}</Text>
        </View>
      );

    case 'error':
      return (
        <View style={[styles.eventCard, styles.errorCard]}>
          <Text style={styles.eventRole}>Error</Text>
          <Text style={[styles.eventText, styles.errorText]}>{event.message}</Text>
        </View>
      );

    case 'tool_use':
      return (
        <View style={[styles.eventCard, styles.toolCard]}>
          <Text style={styles.eventRole}>Tool: {event.name}</Text>
          <Text style={styles.codeText} numberOfLines={4}>
            {JSON.stringify(event.input, null, 2)}
          </Text>
        </View>
      );

    case 'tool_result':
      return (
        <View style={[styles.eventCard, styles.toolResultCard]}>
          <Text style={styles.eventRole}>
            Result{event.name ? `: ${event.name}` : ''}
            {event.isError === true ? ' (error)' : ''}
          </Text>
          <Text style={styles.codeText} numberOfLines={6}>
            {typeof event.output === 'string'
              ? event.output
              : JSON.stringify(event.output, null, 2)}
          </Text>
        </View>
      );

    case 'permission_requested':
      return (
        <View style={[styles.eventCard, styles.permissionCard]}>
          <Text style={styles.eventRole}>Permission Required: {event.toolName}</Text>
          <Text style={styles.codeText} numberOfLines={4}>
            {JSON.stringify(event.input, null, 2)}
          </Text>
          <View style={styles.permissionBtns}>
            <Pressable
              style={[styles.permissionBtn, styles.allowBtn]}
              onPress={() => onResolve(event.requestId, 'allow')}
            >
              <Text style={styles.allowBtnText}>Allow</Text>
            </Pressable>
            <Pressable
              style={[styles.permissionBtn, styles.denyBtn]}
              onPress={() => onResolve(event.requestId, 'deny')}
            >
              <Text style={styles.denyBtnText}>Deny</Text>
            </Pressable>
          </View>
        </View>
      );

    case 'status':
      return (
        <View style={styles.statusRow}>
          <View style={[styles.statusPill, { backgroundColor: statusColor(event.status).bg }]}>
            <Text style={[styles.statusPillText, { color: statusColor(event.status).text }]}>
              {event.status}
            </Text>
          </View>
        </View>
      );

    case 'turn_complete':
      return (
        <View style={styles.statusRow}>
          <Text style={styles.metaText}>
            Turn complete
            {event.costUsd !== undefined ? ` · ${formatCost(event.costUsd)}` : ''}
            {event.inputTokens !== undefined
              ? ` · ${event.inputTokens}↑ ${event.outputTokens ?? 0}↓ tokens`
              : ''}
          </Text>
        </View>
      );

    default:
      return null;
  }
}

export default function RunDetailScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const [message, setMessage] = useState('');
  const flatListRef = useRef<FlatList<RunEventRow>>(null);

  const { data: run, isLoading } = useRun(id);
  useRunEventStream(id);

  const sendMsg = useSendMessage(id);
  const cancelRun = useCancelRun(id);
  const resolvePermission = useResolvePermission(id);
  const suspendSprite = useSuspendSprite(run?.spriteId ?? '');
  const resumeSprite = useResumeSprite(run?.spriteId ?? '');

  const isActive =
    run?.status === 'running' || run?.status === 'starting' || run?.status === 'awaiting_input';
  const canSend = run?.status === 'awaiting_input';

  async function handleSend() {
    const text = message.trim();
    if (!text) return;
    setMessage('');
    try {
      await sendMsg.mutateAsync(text);
    } catch (e) {
      Alert.alert('Error', e instanceof Error ? e.message : 'Failed to send message.');
    }
  }

  async function handleCancel() {
    Alert.alert('Cancel Run', 'Are you sure you want to cancel this run?', [
      { text: 'No', style: 'cancel' },
      {
        text: 'Cancel Run',
        style: 'destructive',
        onPress: async () => {
          try {
            await cancelRun.mutateAsync();
          } catch (e) {
            Alert.alert('Error', e instanceof Error ? e.message : 'Failed to cancel.');
          }
        },
      },
    ]);
  }

  function handleResolve(requestId: string, decision: 'allow' | 'deny') {
    void resolvePermission.mutateAsync({ requestId, body: { decision } });
  }

  const renderItem = useCallback(
    ({ item }: { item: RunEventRow }) => (
      <EventCard row={item} runId={id} onResolve={handleResolve} />
    ),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [id],
  );

  const keyExtractor = useCallback((item: RunEventRow) => String(item.seq), []);

  if (isLoading || !run) {
    return (
      <View style={styles.center}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  const color = statusColor(run.status);

  return (
    <View style={styles.container}>
      {/* Header info */}
      <View style={styles.header}>
        <View style={styles.headerTop}>
          <View style={[styles.badge, { backgroundColor: color.bg }]}>
            <Text style={[styles.badgeText, { color: color.text }]}>{color.label}</Text>
          </View>
          <View style={styles.headerActions}>
            {isActive && (
              <Pressable style={styles.actionBtn} onPress={() => void handleCancel()}>
                <Text style={styles.actionBtnText}>Cancel</Text>
              </Pressable>
            )}
            {run.spriteId && run.status !== 'running' && (
              <Pressable style={styles.actionBtn} onPress={() => void suspendSprite.mutateAsync()}>
                <Text style={styles.actionBtnText}>Suspend</Text>
              </Pressable>
            )}
            {run.spriteId && (
              <Pressable
                style={[styles.actionBtn, styles.actionBtnBlue]}
                onPress={() => void resumeSprite.mutateAsync()}
              >
                <Text style={[styles.actionBtnText, styles.actionBtnBlueText]}>Resume</Text>
              </Pressable>
            )}
          </View>
        </View>
        <Text style={styles.promptText} numberOfLines={3}>
          {run.prompt}
        </Text>
        {run.totalCostUsd > 0 && (
          <Text style={styles.totals}>
            {formatCost(run.totalCostUsd)} · {run.totalInputTokens}↑ {run.totalOutputTokens}↓
          </Text>
        )}
      </View>

      {/* Events list */}
      <FlatList
        ref={flatListRef}
        data={run.events}
        renderItem={renderItem}
        keyExtractor={keyExtractor}
        contentContainerStyle={styles.eventsList}
        onContentSizeChange={() => flatListRef.current?.scrollToEnd({ animated: true })}
        ListEmptyComponent={
          <Text style={styles.emptyEvents}>No events yet. The run is starting…</Text>
        }
      />

      {/* Composer */}
      <View style={styles.composer}>
        <TextInput
          style={styles.composerInput}
          value={message}
          onChangeText={setMessage}
          multiline
          placeholder={canSend ? 'Send a message…' : 'Waiting for agent…'}
          placeholderTextColor="#9CA3AF"
          editable={canSend}
        />
        <Pressable
          style={[styles.sendBtn, (!canSend || !message.trim()) && styles.sendBtnDisabled]}
          onPress={() => void handleSend()}
          disabled={!canSend || !message.trim() || sendMsg.isPending}
        >
          {sendMsg.isPending ? (
            <ActivityIndicator color="#FFFFFF" size="small" />
          ) : (
            <Text style={styles.sendBtnText}>Send</Text>
          )}
        </Pressable>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: '#F9FAFB' },
  center: { flex: 1, alignItems: 'center', justifyContent: 'center' },
  header: {
    backgroundColor: '#FFFFFF',
    padding: 16,
    borderBottomWidth: 1,
    borderBottomColor: '#E5E7EB',
    gap: 8,
  },
  headerTop: { flexDirection: 'row', alignItems: 'center', gap: 8 },
  headerActions: { flexDirection: 'row', gap: 8, marginLeft: 'auto' },
  badge: { paddingHorizontal: 8, paddingVertical: 3, borderRadius: 999 },
  badgeText: { fontSize: 12, fontWeight: '600' },
  actionBtn: {
    paddingHorizontal: 10,
    paddingVertical: 5,
    borderRadius: 6,
    borderWidth: 1,
    borderColor: '#D1D5DB',
  },
  actionBtnBlue: { borderColor: '#2563EB' },
  actionBtnText: { fontSize: 12, color: '#374151', fontWeight: '500' },
  actionBtnBlueText: { color: '#2563EB' },
  promptText: { fontSize: 14, color: '#111827', lineHeight: 20 },
  totals: { fontSize: 12, color: '#9CA3AF' },
  eventsList: { padding: 12, gap: 10 },
  eventCard: { borderRadius: 10, padding: 12, gap: 6 },
  assistantCard: { backgroundColor: '#FFFFFF', borderLeftWidth: 3, borderLeftColor: '#2563EB' },
  userCard: { backgroundColor: '#EFF6FF', borderLeftWidth: 3, borderLeftColor: '#60A5FA' },
  errorCard: { backgroundColor: '#FEF2F2', borderLeftWidth: 3, borderLeftColor: '#EF4444' },
  toolCard: { backgroundColor: '#F3F4F6', borderLeftWidth: 3, borderLeftColor: '#8B5CF6' },
  toolResultCard: {
    backgroundColor: '#F0FDF4',
    borderLeftWidth: 3,
    borderLeftColor: '#22C55E',
  },
  permissionCard: {
    backgroundColor: '#FFFBEB',
    borderLeftWidth: 3,
    borderLeftColor: '#F59E0B',
  },
  eventRole: { fontSize: 11, fontWeight: '700', color: '#6B7280', textTransform: 'uppercase' },
  eventText: { fontSize: 14, color: '#111827', lineHeight: 20 },
  errorText: { color: '#DC2626' },
  codeText: { fontSize: 12, color: '#374151', fontFamily: 'monospace', lineHeight: 18 },
  permissionBtns: { flexDirection: 'row', gap: 8, marginTop: 4 },
  permissionBtn: { flex: 1, paddingVertical: 8, borderRadius: 6, alignItems: 'center' },
  allowBtn: { backgroundColor: '#D1FAE5' },
  denyBtn: { backgroundColor: '#FEE2E2' },
  allowBtnText: { color: '#065F46', fontWeight: '600', fontSize: 13 },
  denyBtnText: { color: '#991B1B', fontWeight: '600', fontSize: 13 },
  statusRow: { alignItems: 'center', paddingVertical: 4 },
  statusPill: { paddingHorizontal: 10, paddingVertical: 3, borderRadius: 999 },
  statusPillText: { fontSize: 11, fontWeight: '600' },
  metaText: { fontSize: 11, color: '#9CA3AF' },
  emptyEvents: { textAlign: 'center', color: '#9CA3AF', paddingTop: 40 },
  composer: {
    flexDirection: 'row',
    padding: 12,
    gap: 8,
    backgroundColor: '#FFFFFF',
    borderTopWidth: 1,
    borderTopColor: '#E5E7EB',
  },
  composerInput: {
    flex: 1,
    borderWidth: 1,
    borderColor: '#D1D5DB',
    borderRadius: 10,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontSize: 15,
    color: '#111827',
    maxHeight: 120,
  },
  sendBtn: {
    backgroundColor: '#2563EB',
    borderRadius: 10,
    paddingHorizontal: 16,
    alignItems: 'center',
    justifyContent: 'center',
    minWidth: 60,
  },
  sendBtnDisabled: { backgroundColor: '#BFDBFE' },
  sendBtnText: { color: '#FFFFFF', fontWeight: '600', fontSize: 15 },
});
