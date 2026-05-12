import { useCallback, useState, useMemo, useEffect, useRef } from 'react';
import { RefreshControl, useWindowDimensions } from 'react-native';
import { useRouter, useLocalSearchParams } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Heading } from '@/components/ui/heading';
import { Pressable } from '@/components/ui/pressable';
import { FlatList } from '@/components/ui/flat-list';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Button, ButtonText } from '@/components/ui/button';

import { useRuns } from '../../src/hooks/useRuns';
import { useDebounced } from '../../src/hooks/useDebounced';
import { relativeTime, truncate, statusColor, formatCost } from '../../src/utils';
import { StaggerEnter, usePulse } from '../../src/animations';
import Animated from 'react-native-reanimated';
import type { RunRow } from '../../src/api';
import { RunsFilterBar } from '../../components/runs-filter-bar';
import type { RunStatus } from '../../components/runs-filter-bar';

function parseStatusParam(raw: string | string[] | undefined): ReadonlySet<RunStatus> {
  if (!raw || (Array.isArray(raw) && raw.length === 0)) return new Set();
  const str = Array.isArray(raw) ? raw[0] : raw;
  if (!str) return new Set();
  return new Set(str.split(',').map((s) => s.trim()).filter(Boolean) as RunStatus[]);
}

function serializeStatuses(statuses: ReadonlySet<RunStatus>): string | undefined {
  return statuses.size === 0 ? undefined : Array.from(statuses).join(',');
}

function filterRuns(
  runs: RunRow[],
  query: string,
  statuses: ReadonlySet<RunStatus>,
): RunRow[] {
  const q = query.trim().toLowerCase();
  return runs.filter((run) => {
    if (statuses.size > 0 && !statuses.has(run.status as RunStatus)) return false;
    if (!q) return true;
    return (run.title ?? '').toLowerCase().includes(q) || run.prompt.toLowerCase().includes(q);
  });
}

function runDisplay(run: RunRow) {
  const rawTitle = (run.title ?? '').trim();
  const rawPrompt = run.prompt.trim();
  const titleIsPrompt = rawTitle.length > 0 && rawTitle.toLowerCase() === rawPrompt.toLowerCase();
  return {
    title: rawTitle && !titleIsPrompt ? rawTitle : truncate(rawPrompt, 92),
    preview: rawTitle && !titleIsPrompt ? rawPrompt : '',
  };
}

function StatusDot({ run }: { run: RunRow }) {
  const color = statusColor(run.status);
  const pulseStyle = usePulse(run.status === 'running' || run.status === 'starting');
  return (
    <HStack space="xs" className="items-center">
      <Animated.View style={pulseStyle} className={`w-2 h-2 rounded-full ${color.dotClass}`} />
      <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
        {color.label}
      </Text>
    </HStack>
  );
}

function RunRowItem({ run, index }: { run: RunRow; index: number }) {
  const router = useRouter();
  const { title, preview } = runDisplay(run);
  const agent = run.model ?? 'Claude';

  return (
    <StaggerEnter delay={Math.min(index, 8) * 25}>
      <Pressable
        onPress={() => router.push(`/run/${run.id}`)}
        className="group border-t border-border px-5 py-4 data-[hover=true]:bg-accent/70 data-[active=true]:bg-accent"
      >
        <HStack space="md" className="items-center">
          <Box className="w-[96px]">
            <StatusDot run={run} />
          </Box>
          <VStack className="flex-1 min-w-0" space="xs">
            <Text size="sm" className="text-foreground" numberOfLines={1}>
              {title}
            </Text>
            {preview ? (
              <Text size="xs" className="text-muted-foreground" numberOfLines={1}>
                {preview}
              </Text>
            ) : null}
          </VStack>
          <Text size="xs" className="hidden md:flex w-[128px] text-muted-foreground" numberOfLines={1}>
            {agent}
          </Text>
          <Text size="xs" className="w-[72px] text-muted-foreground">
            {relativeTime(run.updatedAt).replace(' ago', '')}
          </Text>
          <Text size="xs" className="hidden sm:flex w-[72px] text-foreground">
            {run.totalCostUsd > 0 ? formatCost(run.totalCostUsd) : '—'}
          </Text>
        </HStack>
      </Pressable>
    </StaggerEnter>
  );
}

export default function RunsScreen() {
  const router = useRouter();
  const { data, isLoading, refetch, isRefetching } = useRuns();
  const { width } = useWindowDimensions();
  const showWideColumns = width >= 820;

  const params = useLocalSearchParams<{ q?: string; status?: string }>();
  const [query, setQuery] = useState<string>(
    () => (Array.isArray(params.q) ? (params.q[0] ?? '') : (params.q ?? '')),
  );
  const [selectedStatuses, setSelectedStatuses] = useState<ReadonlySet<RunStatus>>(
    () => parseStatusParam(params.status),
  );

  const debouncedQuery = useDebounced(query, 200);
  const didMountRef = useRef(false);
  useEffect(() => {
    if (!didMountRef.current) {
      didMountRef.current = true;
      return;
    }
    router.setParams({
      q: debouncedQuery || undefined,
      status: serializeStatuses(selectedStatuses),
    } as Record<string, string | undefined>);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedQuery, selectedStatuses]);

  const allRuns = data?.runs ?? [];
  const filteredRuns = useMemo(
    () => filterRuns(allRuns, debouncedQuery, selectedStatuses),
    [allRuns, debouncedQuery, selectedStatuses],
  );
  const isFiltered = debouncedQuery.trim().length > 0 || selectedStatuses.size > 0;

  const renderItem = useCallback(
    ({ item, index }: { item: RunRow; index: number }) => <RunRowItem run={item} index={index} />,
    [],
  );
  const keyExtractor = useCallback((item: RunRow) => item.id, []);

  function clearFilters() {
    setQuery('');
    setSelectedStatuses(new Set());
    router.setParams({ q: undefined, status: undefined } as Record<string, string | undefined>);
  }

  if (!isLoading && allRuns.length === 0) {
    return (
      <Box className="flex-1 bg-background items-center justify-center p-6">
        <Button onPress={() => router.push('/new')} className="px-6">
          <ButtonText>+ New chat</ButtonText>
        </Button>
      </Box>
    );
  }

  return (
    <Box className="flex-1 bg-background">
      <FlatList
        data={filteredRuns}
        renderItem={renderItem}
        keyExtractor={keyExtractor}
        refreshControl={<RefreshControl refreshing={isRefetching || isLoading} onRefresh={refetch} />}
        contentContainerStyle={{ padding: 24, paddingBottom: 96, maxWidth: 1240, width: '100%', alignSelf: 'center' }}
        ListHeaderComponent={
          <VStack space="xl" className="mb-8">
            <HStack className="items-start justify-between gap-4">
              <VStack space="xs" className="flex-1">
                <Heading className="text-foreground text-5xl leading-tight">
                  Chats
                </Heading>
                <Text size="md" className="text-muted-foreground">
                  Search, filter, and resume every active HowlerForge session.
                </Text>
              </VStack>
              <Button onPress={() => router.push('/new')} className="mt-2 px-5">
                <ButtonText>+ New chat</ButtonText>
              </Button>
            </HStack>

            <Box className="bg-card border border-border rounded-2xl overflow-hidden">
              <RunsFilterBar
                query={query}
                onQueryChange={setQuery}
                selectedStatuses={selectedStatuses}
                onStatusChange={setSelectedStatuses}
              />
            </Box>

            <Box className="bg-card border border-border rounded-2xl overflow-hidden">
              {showWideColumns ? (
                <HStack className="px-5 py-4 items-center">
                  <Text size="xs" className="w-[96px] text-muted-foreground">Status</Text>
                  <Text size="xs" className="flex-1 text-muted-foreground">Title</Text>
                  <Text size="xs" className="w-[128px] text-muted-foreground">Agent</Text>
                  <Text size="xs" className="w-[72px] text-muted-foreground">Updated</Text>
                  <Text size="xs" className="w-[72px] text-muted-foreground">Cost</Text>
                </HStack>
              ) : null}
            </Box>
          </VStack>
        }
        ListFooterComponent={<Box className="h-8" />}
        ListEmptyComponent={
          <VStack className="items-center pt-12" space="sm">
            <Text size="md" className="text-muted-foreground text-center">
              {isLoading
                ? 'Loading…'
                : `No chats match${debouncedQuery.trim() ? ` “${debouncedQuery.trim()}”` : ' the selected filters'}.`}
            </Text>
            {!isLoading && isFiltered ? (
              <Pressable onPress={clearFilters}>
                <Text size="sm" className="text-primary underline">
                  Clear filters
                </Text>
              </Pressable>
            ) : null}
          </VStack>
        }
      />
    </Box>
  );
}
