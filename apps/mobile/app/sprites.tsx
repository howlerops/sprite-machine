import { useState, useCallback } from 'react';
import { FlatList, RefreshControl } from 'react-native';
import { useColorScheme } from 'react-native';
import Animated from 'react-native-reanimated';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import { Spinner } from '@/components/ui/spinner';
import { Heading } from '@/components/ui/heading';
import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogBody,
  AlertDialogFooter,
} from '@/components/ui/alert-dialog';

import {
  useSprites,
  useSuspendSprite,
  useResumeSprite,
  useDeleteSprite,
} from '../src/hooks/useSprites';
import { statusColor, relativeTime, truncate, formatCost } from '../src/utils';
import { StaggerEnter, usePulse } from '../src/animations';
import type { SpriteRow } from '../src/api';

// ---------------------------------------------------------------------------
// StatusPill
// ---------------------------------------------------------------------------

function StatusPill({ status }: { status: string }) {
  const color = statusColor(status);
  const pulseStyle = usePulse(status === 'running' || status === 'starting');
  return (
    <Animated.View style={pulseStyle} className={`px-2 py-0.5 rounded-full ${color.bgClass}`}>
      <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
        {color.label}
      </Text>
    </Animated.View>
  );
}

// ---------------------------------------------------------------------------
// SpriteCard
// ---------------------------------------------------------------------------

function SpriteCard({
  sprite,
  index,
  onDeletePress,
}: {
  sprite: SpriteRow;
  index: number;
  onDeletePress: (sprite: SpriteRow) => void;
}) {
  const suspend = useSuspendSprite(sprite.id);
  const resume = useResumeSprite(sprite.id);

  const canSuspend =
    sprite.status === 'running' || sprite.status === 'starting' || sprite.status === 'creating';
  const canResume = sprite.status === 'suspended';

  return (
    <StaggerEnter delay={Math.min(index, 8) * 35}>
      <Box className="bg-card border border-border rounded-xl p-4 mb-3">
        <VStack space="sm">
          {/* Top row: ID + status pill */}
          <HStack space="sm" className="items-center">
            <Text size="xs" className="text-muted-foreground font-mono flex-1" numberOfLines={1}>
              {truncate(sprite.id, 28)}
            </Text>
            <StatusPill status={sprite.status} />
          </HStack>

          {/* Name if present and different from ID */}
          {sprite.name && sprite.name !== sprite.id ? (
            <Text size="sm" bold className="text-foreground" numberOfLines={1}>
              {sprite.name}
            </Text>
          ) : null}

          {/* Meta row: updated-at + cost */}
          <HStack space="sm" className="items-center">
            <Text size="2xs" className="text-muted-foreground">
              {relativeTime(sprite.updatedAt)}
            </Text>
            {sprite.model ? (
              <Text size="2xs" className="text-muted-foreground">
                · {sprite.model}
              </Text>
            ) : null}
          </HStack>

          {/* Action row */}
          <HStack space="sm" className="items-center pt-1">
            {canSuspend ? (
              <Button
                variant="outline"
                size="sm"
                isDisabled={suspend.isPending}
                onPress={() => void suspend.mutateAsync()}
              >
                {suspend.isPending ? <ButtonSpinner /> : null}
                <ButtonText>Suspend</ButtonText>
              </Button>
            ) : null}
            {canResume ? (
              <Button
                variant="default"
                size="sm"
                isDisabled={resume.isPending}
                onPress={() => void resume.mutateAsync()}
              >
                {resume.isPending ? <ButtonSpinner /> : null}
                <ButtonText>Resume</ButtonText>
              </Button>
            ) : null}
            <Box className="flex-1" />
            <Button
              variant="outline"
              size="sm"
              onPress={() => onDeletePress(sprite)}
              className="border-destructive"
            >
              <ButtonText className="text-destructive">Delete</ButtonText>
            </Button>
          </HStack>
        </VStack>
      </Box>
    </StaggerEnter>
  );
}

// ---------------------------------------------------------------------------
// DeleteConfirmDialog
// ---------------------------------------------------------------------------

function DeleteConfirmDialog({
  sprite,
  onClose,
  onConfirm,
  isPending,
}: {
  sprite: SpriteRow | null;
  onClose: () => void;
  onConfirm: () => void;
  isPending: boolean;
}) {
  return (
    <AlertDialog isOpen={!!sprite} onClose={onClose}>
      <AlertDialogBackdrop />
      <AlertDialogContent>
        <AlertDialogHeader>
          <Heading size="md" bold className="text-foreground">
            Delete sprite?
          </Heading>
        </AlertDialogHeader>
        <AlertDialogBody>
          <VStack space="sm">
            <Text size="sm" className="text-muted-foreground">
              This will permanently destroy the sprite and free its resources. Any chats attached to
              this sprite will remain in your history but can no longer resume.
            </Text>
            {sprite ? (
              <Text size="xs" className="font-mono text-muted-foreground">
                {sprite.id}
              </Text>
            ) : null}
          </VStack>
        </AlertDialogBody>
        <AlertDialogFooter>
          <Button variant="outline" size="sm" onPress={onClose} isDisabled={isPending}>
            <ButtonText>Keep sprite</ButtonText>
          </Button>
          <Button variant="destructive" size="sm" isDisabled={isPending} onPress={onConfirm}>
            {isPending ? <ButtonSpinner /> : null}
            <ButtonText>Delete</ButtonText>
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

// ---------------------------------------------------------------------------
// SpritesScreen — the exported screen
// ---------------------------------------------------------------------------

export default function SpritesScreen() {
  const { data, isLoading, refetch, isRefetching } = useSprites();
  const sprites: SpriteRow[] = data?.sprites ?? [];

  const [pendingDelete, setPendingDelete] = useState<SpriteRow | null>(null);
  const deleteSpriteMutation = useDeleteSprite(pendingDelete?.id ?? '');

  const handleDeletePress = useCallback((sprite: SpriteRow) => {
    setPendingDelete(sprite);
  }, []);

  const handleDeleteConfirm = useCallback(async () => {
    if (!pendingDelete) return;
    try {
      await deleteSpriteMutation.mutateAsync();
    } finally {
      setPendingDelete(null);
    }
  }, [pendingDelete, deleteSpriteMutation]);

  const handleDeleteClose = useCallback(() => {
    setPendingDelete(null);
  }, []);

  const renderItem = useCallback(
    ({ item, index }: { item: SpriteRow; index: number }) => (
      <SpriteCard sprite={item} index={index} onDeletePress={handleDeletePress} />
    ),
    [handleDeletePress],
  );

  const keyExtractor = useCallback((item: SpriteRow) => item.id, []);

  if (isLoading) {
    return (
      <Box className="flex-1 items-center justify-center bg-background">
        <Spinner size="large" />
      </Box>
    );
  }

  return (
    <Box className="flex-1 bg-background">
      <FlatList
        data={sprites}
        renderItem={renderItem}
        keyExtractor={keyExtractor}
        contentContainerStyle={{ padding: 16, paddingBottom: 32 }}
        refreshControl={
          <RefreshControl refreshing={isRefetching} onRefresh={() => void refetch()} />
        }
        ListEmptyComponent={
          <Box className="flex-1 items-center justify-center pt-24">
            <Text size="sm" className="text-muted-foreground text-center">
              No sprites yet. Start a new chat to provision one.
            </Text>
          </Box>
        }
      />

      <DeleteConfirmDialog
        sprite={pendingDelete}
        onClose={handleDeleteClose}
        onConfirm={() => void handleDeleteConfirm()}
        isPending={deleteSpriteMutation.isPending}
      />
    </Box>
  );
}
