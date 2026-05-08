import { useCallback } from 'react';
import {
  View,
  Text,
  FlatList,
  TouchableOpacity,
  RefreshControl,
  StyleSheet,
  Pressable,
} from 'react-native';
import { useRouter } from 'expo-router';
import { useRuns } from '../../src/hooks/useRuns';
import { relativeTime, truncate, statusColor, formatCost } from '../../src/utils';
import type { RunRow } from '../../src/api';

function RunCard({ run }: { run: RunRow }) {
  const router = useRouter();
  const color = statusColor(run.status);

  return (
    <TouchableOpacity
      style={styles.card}
      onPress={() => router.push(`/run/${run.id}`)}
      activeOpacity={0.7}
    >
      <View style={styles.cardHeader}>
        <View style={[styles.badge, { backgroundColor: color.bg }]}>
          <Text style={[styles.badgeText, { color: color.text }]}>{color.label}</Text>
        </View>
        <Text style={styles.time}>{relativeTime(run.updatedAt)}</Text>
      </View>
      <Text style={styles.prompt}>{truncate(run.prompt, 120)}</Text>
      {run.totalCostUsd > 0 && <Text style={styles.cost}>{formatCost(run.totalCostUsd)}</Text>}
    </TouchableOpacity>
  );
}

export default function RunsScreen() {
  const router = useRouter();
  const { data, isLoading, refetch, isRefetching } = useRuns();

  const renderItem = useCallback(({ item }: { item: RunRow }) => <RunCard run={item} />, []);
  const keyExtractor = useCallback((item: RunRow) => item.id, []);

  return (
    <View style={styles.container}>
      <FlatList
        data={data?.runs ?? []}
        renderItem={renderItem}
        keyExtractor={keyExtractor}
        contentContainerStyle={styles.list}
        refreshControl={
          <RefreshControl refreshing={isRefetching || isLoading} onRefresh={refetch} />
        }
        ListEmptyComponent={
          <View style={styles.empty}>
            <Text style={styles.emptyText}>
              {isLoading ? 'Loading...' : 'No runs yet. Tap + to create one.'}
            </Text>
          </View>
        }
      />
      <Pressable style={styles.fab} onPress={() => router.push('/new')}>
        <Text style={styles.fabText}>+</Text>
      </Pressable>
    </View>
  );
}

const styles = StyleSheet.create({
  container: { flex: 1, backgroundColor: '#F9FAFB' },
  list: { padding: 16, gap: 12 },
  card: {
    backgroundColor: '#FFFFFF',
    borderRadius: 12,
    padding: 16,
    shadowColor: '#000',
    shadowOffset: { width: 0, height: 1 },
    shadowOpacity: 0.06,
    shadowRadius: 4,
    elevation: 2,
    gap: 8,
  },
  cardHeader: { flexDirection: 'row', alignItems: 'center', gap: 8 },
  badge: { paddingHorizontal: 8, paddingVertical: 3, borderRadius: 999 },
  badgeText: { fontSize: 12, fontWeight: '600' },
  time: { fontSize: 12, color: '#9CA3AF', marginLeft: 'auto' },
  prompt: { fontSize: 14, color: '#111827', lineHeight: 20 },
  cost: { fontSize: 12, color: '#6B7280' },
  empty: { flex: 1, alignItems: 'center', paddingTop: 80 },
  emptyText: { color: '#9CA3AF', fontSize: 16 },
  fab: {
    position: 'absolute',
    top: 16,
    right: 16,
    width: 48,
    height: 48,
    borderRadius: 24,
    backgroundColor: '#2563EB',
    alignItems: 'center',
    justifyContent: 'center',
    shadowColor: '#2563EB',
    shadowOffset: { width: 0, height: 4 },
    shadowOpacity: 0.3,
    shadowRadius: 8,
    elevation: 6,
  },
  fabText: { color: '#FFFFFF', fontSize: 24, fontWeight: '300', lineHeight: 28 },
});
