import { useState, useRef, useCallback } from 'react';
import type { FlatList as RNFlatListType } from 'react-native';
import { Platform } from 'react-native';
import { useLocalSearchParams, useRouter } from 'expo-router';
import { tva } from '@gluestack-ui/utils/nativewind-utils';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { RNFlatList as FlatList } from '@/components/ui/flat-list';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Spinner } from '@/components/ui/spinner';
import { Input, InputField } from '@/components/ui/input';
import { Textarea, TextareaInput } from '@/components/ui/textarea';
import { Button, ButtonText, ButtonSpinner } from '@/components/ui/button';
import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogBody,
  AlertDialogFooter,
} from '@/components/ui/alert-dialog';
import { Heading } from '@/components/ui/heading';
import { useToast, Toast, ToastTitle, ToastDescription } from '@/components/ui/toast';

import {
  useRun,
  useSendMessage,
  useCancelRun,
  useResolvePermission,
} from '../../src/hooks/useRuns';
import { useSprite, useSuspendSprite, useResumeSprite } from '../../src/hooks/useSprites';
import { useRunEventStream } from '../../src/sse';
import { statusColor, formatCost } from '../../src/utils';
import { usePulse, CrossfadePill, TabCrossfade } from '../../src/animations';
import Animated from 'react-native-reanimated';
import type { RunEventRow } from '../../src/api';

// Platform-split shell panel: Metro's platform-extension resolver picks
// shell-panel.web.tsx when bundling for web and shell-panel.native.tsx for
// native. A plain static import is sufficient — wterm only lives in
// shell-panel.web.tsx, so it never lands in the native bundle.
import ShellPanel from '../../components/shell-panel';

const eventCardStyle = tva({
  base: 'rounded-[10px] p-3 border-l-4',
  variants: {
    kind: {
      assistant: 'bg-card border border-border border-l-primary',
      user: 'bg-status-running-muted border-l-status-running',
      error: 'bg-destructive/10 border-l-destructive',
      tool: 'bg-muted border-l-status-tool',
      toolResult: 'bg-status-success-muted border-l-status-success',
      permission: 'bg-status-warning-muted border-l-status-warning',
    },
  },
});

const tabChipStyle = tva({
  base: 'px-3 py-1.5 rounded-full border',
  variants: {
    active: {
      true: 'border-primary bg-primary',
      false: 'border-border bg-background data-[hover=true]:bg-accent',
    },
  },
});

type ActiveTab = 'timeline' | 'shell';

function EventCard({
  row,
  onResolve,
}: {
  row: RunEventRow;
  onResolve: (requestId: string, decision: 'allow' | 'deny') => void;
}) {
  const { event } = row;

  switch (event.type) {
    case 'assistant_text':
      return (
        <Box className={eventCardStyle({ kind: 'assistant' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              Assistant
            </Text>
            <Text size="sm" className="text-foreground">
              {event.text}
            </Text>
          </VStack>
        </Box>
      );

    case 'user_message':
      return (
        <Box className={eventCardStyle({ kind: 'user' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              You
            </Text>
            <Text size="sm" className="text-foreground">
              {event.text}
            </Text>
          </VStack>
        </Box>
      );

    case 'error':
      return (
        <Box className={eventCardStyle({ kind: 'error' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              Error
            </Text>
            <Text size="sm" className="text-destructive">
              {event.message}
            </Text>
          </VStack>
        </Box>
      );

    case 'tool_use':
      return (
        <Box className={eventCardStyle({ kind: 'tool' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              Tool: {event.name}
            </Text>
            <Text size="xs" numberOfLines={4} className="text-foreground/80 font-mono">
              {JSON.stringify(event.input, null, 2)}
            </Text>
          </VStack>
        </Box>
      );

    case 'tool_result':
      return (
        <Box className={eventCardStyle({ kind: 'toolResult' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              Result{event.name ? `: ${event.name}` : ''}
              {event.isError === true ? ' (error)' : ''}
            </Text>
            <Text size="xs" numberOfLines={6} className="text-foreground/80 font-mono">
              {typeof event.output === 'string'
                ? event.output
                : JSON.stringify(event.output, null, 2)}
            </Text>
          </VStack>
        </Box>
      );

    case 'permission_requested':
      return (
        <Box className={eventCardStyle({ kind: 'permission' })}>
          <VStack space="sm">
            <Text size="2xs" bold className="uppercase text-muted-foreground">
              Permission Required: {event.toolName}
            </Text>
            <Text size="xs" numberOfLines={4} className="text-foreground/80 font-mono">
              {JSON.stringify(event.input, null, 2)}
            </Text>
            <HStack space="sm">
              <Button
                variant="default"
                size="sm"
                className="flex-1"
                onPress={() => onResolve(event.requestId, 'allow')}
              >
                <ButtonText>Allow</ButtonText>
              </Button>
              <Button
                variant="destructive"
                size="sm"
                className="flex-1"
                onPress={() => onResolve(event.requestId, 'deny')}
              >
                <ButtonText>Deny</ButtonText>
              </Button>
            </HStack>
          </VStack>
        </Box>
      );

    case 'status': {
      const color = statusColor(event.status);
      return (
        <Box className="items-center py-1">
          <Box className={`px-2.5 py-0.5 rounded-full ${color.bgClass}`}>
            <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
              {event.status}
            </Text>
          </Box>
        </Box>
      );
    }

    case 'turn_complete':
      return (
        <Box className="items-center py-1">
          <Text size="2xs" className="text-muted-foreground">
            Turn complete
            {event.costUsd !== undefined ? ` · ${formatCost(event.costUsd)}` : ''}
            {event.inputTokens !== undefined
              ? ` · ${event.inputTokens}↑ ${event.outputTokens ?? 0}↓ tokens`
              : ''}
          </Text>
        </Box>
      );

    default:
      return null;
  }
}

export default function RunDetailScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const [message, setMessage] = useState('');
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [activeTab, setActiveTab] = useState<ActiveTab>('timeline');
  const flatListRef = useRef<RNFlatListType<RunEventRow>>(null);
  const toast = useToast();

  const { data: run, isLoading } = useRun(id);
  useRunEventStream(id);

  const sendMsg = useSendMessage(id);
  const cancelRun = useCancelRun(id);
  const resolvePermission = useResolvePermission(id);
  const spriteId = run?.spriteId ?? '';
  const { data: sprite } = useSprite(spriteId);
  const suspendSprite = useSuspendSprite(spriteId);
  const resumeSprite = useResumeSprite(spriteId);

  const isActive =
    run?.status === 'running' || run?.status === 'starting' || run?.status === 'awaiting_input';
  const canSend = run?.status === 'awaiting_input';
  const canSuspend =
    !!run?.spriteId &&
    sprite?.status !== 'suspended' &&
    (run?.status === 'running' || run?.status === 'awaiting_input');
  const canResume = !!run?.spriteId && sprite?.status === 'suspended';

  function showError(messageText: string) {
    const tid = String(Math.random());
    toast.show({
      id: tid,
      placement: 'top',
      duration: 3000,
      render: () => (
        <Toast nativeID={`toast-${tid}`} action="error" variant="solid">
          <ToastTitle>Error</ToastTitle>
          <ToastDescription>{messageText}</ToastDescription>
        </Toast>
      ),
    });
  }

  async function handleSend() {
    const text = message.trim();
    if (!text) return;
    setMessage('');
    try {
      await sendMsg.mutateAsync(text);
    } catch (e) {
      showError(e instanceof Error ? e.message : 'Failed to send message.');
    }
  }

  async function performCancel() {
    setConfirmCancel(false);
    try {
      await cancelRun.mutateAsync();
    } catch (e) {
      showError(e instanceof Error ? e.message : 'Failed to cancel.');
    }
  }

  function handleResolve(requestId: string, decision: 'allow' | 'deny') {
    void resolvePermission.mutateAsync({ requestId, body: { decision } });
  }

  const renderItem = useCallback(
    ({ item }: { item: RunEventRow }) => <EventCard row={item} onResolve={handleResolve} />,
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [id],
  );

  const keyExtractor = useCallback((item: RunEventRow) => String(item.seq), []);

  if (isLoading || !run) {
    return (
      <Box className="flex-1 items-center justify-center bg-background">
        <Spinner size="large" />
      </Box>
    );
  }

  const color = statusColor(run.status);
  const pulseStyle = usePulse(run.status === 'running');

  // Shell tab is only rendered on web; on native it shows the placeholder.
  const showShellTab = Platform.OS === 'web';

  return (
    <Box className="flex-1 bg-background lg:max-w-3xl lg:mx-auto lg:w-full">
      {/* Header info */}
      <Box className="bg-card border-b border-border px-4 py-3 md:px-6 md:py-4 lg:px-8 lg:py-6">
        <VStack space="sm">
          <HStack space="sm" className="items-center">
            <CrossfadePill contentKey={run.status}>
              <Animated.View
                style={pulseStyle}
                className={`px-2 py-0.5 rounded-full ${color.bgClass}`}
              >
                <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
                  {color.label}
                </Text>
              </Animated.View>
            </CrossfadePill>
            <HStack space="xs" className="ml-auto">
              {isActive ? (
                <Button variant="outline" size="sm" onPress={() => setConfirmCancel(true)}>
                  <ButtonText>Cancel</ButtonText>
                </Button>
              ) : null}
              {canSuspend ? (
                <Button
                  variant="outline"
                  size="sm"
                  onPress={() => void suspendSprite.mutateAsync()}
                >
                  <ButtonText>Suspend</ButtonText>
                </Button>
              ) : null}
              {canResume ? (
                <Button variant="default" size="sm" onPress={() => void resumeSprite.mutateAsync()}>
                  <ButtonText>Resume</ButtonText>
                </Button>
              ) : null}
            </HStack>
          </HStack>
          {run.title ? (
            <VStack space="xs">
              <Text size="sm" bold className="text-foreground">
                {run.title}
              </Text>
              <Text size="xs" className="text-muted-foreground">
                {run.prompt}
              </Text>
            </VStack>
          ) : (
            <Text size="sm" className="text-foreground" numberOfLines={3}>
              {run.prompt}
            </Text>
          )}
          {run.totalCostUsd > 0 ? (
            <Text size="xs" className="text-muted-foreground">
              {formatCost(run.totalCostUsd)} · {run.totalInputTokens}↑ {run.totalOutputTokens}↓
            </Text>
          ) : null}

          {/* Tab segment: Timeline | Shell */}
          <HStack space="sm" className="pt-1">
            <Pressable
              onPress={() => setActiveTab('timeline')}
              className={tabChipStyle({ active: activeTab === 'timeline' })}
            >
              <Text
                size="xs"
                bold={activeTab === 'timeline'}
                className={activeTab === 'timeline' ? 'text-primary-foreground' : 'text-foreground'}
              >
                Timeline
              </Text>
            </Pressable>
            {showShellTab ? (
              <Pressable
                onPress={() => setActiveTab('shell')}
                className={tabChipStyle({ active: activeTab === 'shell' })}
              >
                <Text
                  size="xs"
                  bold={activeTab === 'shell'}
                  className={activeTab === 'shell' ? 'text-primary-foreground' : 'text-foreground'}
                >
                  Shell
                </Text>
              </Pressable>
            ) : null}
          </HStack>
        </VStack>
      </Box>

      {/* Tab content */}
      <TabCrossfade contentKey={activeTab}>
        {activeTab === 'shell' ? (
          <ShellPanel spriteId={spriteId} runId={id} sprite={sprite ?? null} />
        ) : (
          <>
            {/* Events list */}
            <FlatList
              ref={flatListRef}
              data={run.events}
              renderItem={renderItem}
              keyExtractor={keyExtractor}
              contentContainerStyle={{ padding: 12, gap: 10 }}
              onContentSizeChange={() => flatListRef.current?.scrollToEnd({ animated: true })}
              ListEmptyComponent={
                <Text size="sm" className="text-muted-foreground text-center pt-10">
                  No events yet. The run is starting…
                </Text>
              }
            />

            {/* Composer */}
            <HStack space="sm" className="p-3 bg-card border-t border-border items-end">
              <Box className="flex-1">
                <Input>
                  <InputField
                    value={message}
                    onChangeText={setMessage}
                    multiline
                    placeholder={canSend ? 'Send a message…' : 'Waiting for agent…'}
                    editable={canSend}
                    className="max-h-32"
                  />
                </Input>
              </Box>
              <Button
                variant="default"
                isDisabled={!canSend || !message.trim() || sendMsg.isPending}
                onPress={() => void handleSend()}
              >
                {sendMsg.isPending ? <ButtonSpinner /> : null}
                <ButtonText>Send</ButtonText>
              </Button>
            </HStack>
          </>
        )}
      </TabCrossfade>

      <AlertDialog isOpen={confirmCancel} onClose={() => setConfirmCancel(false)}>
        <AlertDialogBackdrop />
        <AlertDialogContent>
          <AlertDialogHeader>
            <Heading size="md" bold className="text-foreground">
              Cancel run?
            </Heading>
          </AlertDialogHeader>
          <AlertDialogBody>
            <Text size="sm" className="text-muted-foreground">
              Are you sure you want to cancel this run? Any in-flight work will be lost.
            </Text>
          </AlertDialogBody>
          <AlertDialogFooter>
            <Button variant="outline" size="sm" onPress={() => setConfirmCancel(false)}>
              <ButtonText>Keep running</ButtonText>
            </Button>
            <Button variant="destructive" size="sm" onPress={() => void performCancel()}>
              <ButtonText>Cancel run</ButtonText>
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Box>
  );
}
