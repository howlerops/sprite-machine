import { useCallback } from 'react';
import { RefreshControl, useWindowDimensions } from 'react-native';
import { useRouter } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Heading } from '@/components/ui/heading';
import { Pressable } from '@/components/ui/pressable';
import { FlatList } from '@/components/ui/flat-list';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';

import { useRuns } from '../../src/hooks/useRuns';
import { relativeTime, truncate, statusColor, formatCost } from '../../src/utils';
import { StaggerEnter, usePulse } from '../../src/animations';
import Animated from 'react-native-reanimated';
import type { RunRow } from '../../src/api';

const LG_BREAKPOINT = 1024;

function RunCard({ run, index }: { run: RunRow; index: number }) {
  const router = useRouter();
  const color = statusColor(run.status);
  const pulseStyle = usePulse(run.status === 'running');

  return (
    <StaggerEnter delay={Math.min(index, 8) * 35}>
      <Pressable
        onPress={() => router.push(`/run/${run.id}`)}
        className="bg-card border border-border rounded-[10px] p-4 data-[hover=true]:bg-accent data-[active=true]:bg-accent/80"
      >
        <VStack space="sm">
          <HStack space="sm" className="items-center">
            <Animated.View
              style={pulseStyle}
              className={`px-2 py-0.5 rounded-full ${color.bgClass}`}
            >
              <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
                {color.label}
              </Text>
            </Animated.View>
            <Text size="xs" className="text-muted-foreground ml-auto">
              {relativeTime(run.updatedAt)}
            </Text>
          </HStack>
          <Text size="sm" className="text-foreground">
            {run.title ?? truncate(run.prompt, 120)}
          </Text>
          {run.totalCostUsd > 0 && (
            <Text size="xs" className="text-muted-foreground">
              {formatCost(run.totalCostUsd)}
            </Text>
          )}
        </VStack>
      </Pressable>
    </StaggerEnter>
  );
}

export default function RunsScreen() {
  const router = useRouter();
  const { data, isLoading, refetch, isRefetching } = useRuns();
  const { width } = useWindowDimensions();
  const isDesktop = width >= LG_BREAKPOINT;

  const renderItem = useCallback(
    ({ item, index }: { item: RunRow; index: number }) => <RunCard run={item} index={index} />,
    [],
  );
  const keyExtractor = useCallback((item: RunRow) => item.id, []);

  if (isDesktop) {
    return (
      <Box className="flex-1 bg-background items-center justify-center p-8">
        <VStack space="md" className="max-w-md items-center">
          <Heading size="xl" className="text-foreground text-center">
            Welcome
          </Heading>
          <Text size="md" className="text-muted-foreground text-center">
            Pick a run from the sidebar, or start a new one.
          </Text>
          <Pressable
            onPress={() => router.push('/new')}
            className="mt-4 px-6 py-3 rounded-md bg-primary data-[hover=true]:bg-primary/90 data-[active=true]:bg-primary/80"
          >
            <Text className="text-primary-foreground" size="md" bold>
              + New run
            </Text>
          </Pressable>
        </VStack>
      </Box>
    );
  }

  return (
    <Box className="flex-1 bg-background">
      <FlatList
        data={data?.runs ?? []}
        renderItem={renderItem}
        keyExtractor={keyExtractor}
        contentContainerStyle={{ padding: 16, gap: 12 }}
        refreshControl={
          <RefreshControl refreshing={isRefetching || isLoading} onRefresh={refetch} />
        }
        ListEmptyComponent={
          <VStack className="flex-1 items-center pt-20">
            <Text size="md" className="text-muted-foreground">
              {isLoading ? 'Loading…' : 'No runs yet. Tap + to create one.'}
            </Text>
          </VStack>
        }
      />
      <Pressable
        onPress={() => router.push('/new')}
        className="absolute bottom-6 right-6 w-14 h-14 rounded-full bg-primary items-center justify-center shadow-lg data-[hover=true]:bg-primary/90 data-[active=true]:bg-primary/80"
      >
        <Text className="text-primary-foreground" size="2xl">
          +
        </Text>
      </Pressable>
    </Box>
  );
}
