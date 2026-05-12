/**
 * RunsFilterBar — shared search + status-chip filter UI used by both the
 * mobile chat list (app/(tabs)/index.tsx) and the desktop sidebar chat list
 * (components/sidebar.tsx).
 *
 * Props:
 *   query            current search string (controlled)
 *   onQueryChange    called on every keystroke (debounce lives in consumers)
 *   selectedStatuses set of active status filters (empty = "All")
 *   onStatusChange   called with the new set after a chip tap
 *   compact          when true, reduces vertical padding (sidebar mode)
 */
import React from 'react';
import { ScrollView } from 'react-native';
import { tva } from '@gluestack-ui/utils/nativewind-utils';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { Input, InputField } from '@/components/ui/input';
import { VStack } from '@/components/ui/vstack';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type RunStatus = 'running' | 'awaiting_input' | 'completed' | 'failed' | 'cancelled';

export const ALL_STATUSES: RunStatus[] = [
  'running',
  'awaiting_input',
  'completed',
  'failed',
  'cancelled',
];

/** Human-readable chip labels keyed by status value. */
const STATUS_LABELS: Record<RunStatus, string> = {
  running: 'Running',
  awaiting_input: 'Awaiting input',
  completed: 'Completed',
  failed: 'Failed',
  cancelled: 'Cancelled',
};

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------

const chipStyle = tva({
  base: 'px-3 py-1 rounded-full border',
  variants: {
    active: {
      true: 'border-primary bg-primary',
      false: 'border-border bg-background data-[hover=true]:bg-accent',
    },
  },
});

// ---------------------------------------------------------------------------
// StatusChip
// ---------------------------------------------------------------------------

function StatusChip({
  label,
  active,
  onPress,
}: {
  label: string;
  active: boolean;
  onPress: () => void;
}) {
  return (
    <Pressable onPress={onPress} className={chipStyle({ active })}>
      <Text
        size="xs"
        bold={active}
        className={active ? 'text-primary-foreground' : 'text-foreground'}
      >
        {label}
      </Text>
    </Pressable>
  );
}

// ---------------------------------------------------------------------------
// RunsFilterBar
// ---------------------------------------------------------------------------

export interface RunsFilterBarProps {
  query: string;
  onQueryChange: (q: string) => void;
  selectedStatuses: ReadonlySet<RunStatus>;
  onStatusChange: (next: ReadonlySet<RunStatus>) => void;
  /** Reduces padding for use in the narrow sidebar rail. Default false. */
  compact?: boolean;
}

export function RunsFilterBar({
  query,
  onQueryChange,
  selectedStatuses,
  onStatusChange,
  compact = false,
}: RunsFilterBarProps) {
  const px = compact ? 'px-2' : 'px-3';
  const py = compact ? 'py-2' : 'py-3';

  function handleAllPress() {
    onStatusChange(new Set());
  }

  function handleChipPress(status: RunStatus) {
    const next = new Set(selectedStatuses);
    if (next.has(status)) {
      next.delete(status);
    } else {
      next.add(status);
    }
    onStatusChange(next);
  }

  const allActive = selectedStatuses.size === 0;

  return (
    <VStack className={`${py} gap-2`}>
      {/* Search input */}
      <Box className={px}>
        <Input>
          <InputField
            value={query}
            onChangeText={onQueryChange}
            placeholder="Search chats…"
            clearButtonMode="while-editing"
            autoCapitalize="none"
            autoCorrect={false}
          />
        </Input>
      </Box>

      {/* Status chips — horizontally scrollable */}
      <ScrollView
        horizontal
        showsHorizontalScrollIndicator={false}
        contentContainerStyle={{
          paddingLeft: compact ? 8 : 12,
          paddingRight: compact ? 8 : 12,
          gap: 6,
          flexDirection: 'row',
          alignItems: 'center',
        }}
      >
        <StatusChip label="All" active={allActive} onPress={handleAllPress} />
        {ALL_STATUSES.map((s) => (
          <StatusChip
            key={s}
            label={STATUS_LABELS[s]}
            active={selectedStatuses.has(s)}
            onPress={() => handleChipPress(s)}
          />
        ))}
      </ScrollView>
    </VStack>
  );
}
