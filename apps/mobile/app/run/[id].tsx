import { useState, useRef, useCallback, useMemo, useEffect } from 'react';
import type { FlatList as RNFlatListType } from 'react-native';
import { Platform, Image, Linking, ScrollView } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { useLocalSearchParams, useRouter } from 'expo-router';
import { tva } from '@gluestack-ui/utils/nativewind-utils';
import { Feather } from '@expo/vector-icons';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { RNFlatList as FlatList } from '@/components/ui/flat-list';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Spinner } from '@/components/ui/spinner';
import { Input, InputField } from '@/components/ui/input';
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
import type { RunDetail, RunEventRow } from '../../src/api';
import { uploadAttachment } from '../../src/api';
import { useAppConfig } from '../../src/config-store';
import { tokenizeCode } from '../../src/syntax';

// Platform-split shell panel: Metro's platform-extension resolver picks
// shell-panel.web.tsx when bundling for web and shell-panel.native.tsx for
// native. A plain static import is sufficient — wterm only lives in
// shell-panel.web.tsx, so it never lands in the native bundle.
import ShellPanel from '../../components/shell-panel';

// Lobehub-style chat: user messages right-aligned in a primary bubble,
// assistant messages left-aligned in a card bubble. Tool / permission /
// error events stay as full-width labeled cards.
const bubbleStyle = tva({
  base: 'rounded-2xl px-4 py-3 max-w-[78%] shadow-xs',
  variants: {
    role: {
      user: 'bg-primary self-end rounded-br-sm',
      assistant: 'bg-card border border-border self-start rounded-bl-sm',
    },
  },
});

const eventCardStyle = tva({
  base: 'rounded-xl px-4 py-3 border-l-4 shadow-xs',
  variants: {
    kind: {
      error: 'bg-destructive/10 border-l-destructive',
      tool: 'bg-muted border-l-status-tool',
      toolResult: 'bg-status-success-muted border-l-status-success',
      permission: 'bg-status-warning-muted border-l-status-warning',
    },
  },
});

const tabChipStyle = tva({
  base: 'px-2.5 py-1 rounded-full border',
  variants: {
    active: {
      true: 'border-primary bg-primary',
      false: 'border-border bg-background data-[hover=true]:bg-accent',
    },
  },
});

type ActiveTab = 'timeline' | 'shell';

// ---------------------------------------------------------------------------
// Lightweight hand-rolled markdown renderer for chat bubbles.
// Supports: fenced code blocks, inline code, **bold**, *italic*,
// [link](url), inline data:image, and preserved line-breaks.
// No external markdown dependency — keeps the Metro/RN-Web bundle clean.
// ---------------------------------------------------------------------------

// Block-level token types produced by the first parse pass.
type BlockToken =
  | { kind: 'code_block'; lang: string; code: string }
  | { kind: 'image'; src: string }
  | { kind: 'inline_text'; value: string };

// Inline token types produced by the second parse pass (on inline_text blocks).
type InlineToken =
  | { kind: 'text'; value: string }
  | { kind: 'bold'; value: string }
  | { kind: 'italic'; value: string }
  | { kind: 'code'; value: string }
  | { kind: 'link'; label: string; url: string }
  | { kind: 'linebreak' };

/** Phase 1: split raw text into block-level tokens. */
function tokenizeBlocks(raw: string): BlockToken[] {
  const tokens: BlockToken[] = [];
  // Matches fenced code blocks AND image markdown in one pass.
  // Fenced blocks: ```lang\n...\n``` (lang is optional, block may be empty)
  // Images (three URL forms):
  //   1. data: URIs    — data:image/...;base64,...
  //   2. Absolute URLs — https://... or http://...
  //   3. Root-relative — /runs/...
  const blockRe =
    /```([^\n`]*)\n([\s\S]*?)```|!\[[^\]]*\]\(((?:data:image\/[a-zA-Z0-9.+-]+;base64,|https?:\/\/|\/)[^)\s]+)\)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = blockRe.exec(raw)) !== null) {
    if (m.index > last) {
      tokens.push({ kind: 'inline_text', value: raw.slice(last, m.index) });
    }
    if (m[0].startsWith('```')) {
      // Fenced code block: m[1]=lang, m[2]=code body
      tokens.push({ kind: 'code_block', lang: (m[1] ?? '').trim(), code: m[2] ?? '' });
    } else {
      // Image (data URI, absolute URL, or root-relative path): m[3]=src
      const src = m[3];
      if (src) tokens.push({ kind: 'image', src });
    }
    last = m.index + m[0].length;
  }
  if (last < raw.length) {
    tokens.push({ kind: 'inline_text', value: raw.slice(last) });
  }
  if (tokens.length === 0) {
    tokens.push({ kind: 'inline_text', value: raw });
  }
  return tokens;
}

/** Phase 2: tokenize an inline text segment into styled spans. */
function tokenizeInline(text: string): InlineToken[] {
  const tokens: InlineToken[] = [];
  // Order matters: bold before italic so **b** isn't grabbed as *b*
  const inlineRe = /\*\*([^*]+)\*\*|\*([^*]+)\*|`([^`]+)`|\[([^\]]+)\]\(([^)]+)\)|\n/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = inlineRe.exec(text)) !== null) {
    if (m.index > last) {
      tokens.push({ kind: 'text', value: text.slice(last, m.index) });
    }
    if (m[0] === '\n') {
      tokens.push({ kind: 'linebreak' });
    } else if (m[1] !== undefined) {
      tokens.push({ kind: 'bold', value: m[1] });
    } else if (m[2] !== undefined) {
      tokens.push({ kind: 'italic', value: m[2] });
    } else if (m[3] !== undefined) {
      tokens.push({ kind: 'code', value: m[3] });
    } else if (m[4] !== undefined && m[5] !== undefined) {
      tokens.push({ kind: 'link', label: m[4], url: m[5] });
    }
    last = m.index + m[0].length;
  }
  if (last < text.length) {
    tokens.push({ kind: 'text', value: text.slice(last) });
  }
  return tokens;
}

/** Renders a single inline token as a React element. */
function InlineSpan({
  token,
  textClass,
  index,
}: {
  token: InlineToken;
  textClass: string;
  index: number;
}) {
  switch (token.kind) {
    case 'text':
      return (
        <Text key={index} size="sm" className={textClass}>
          {token.value}
        </Text>
      );
    case 'bold':
      return (
        <Text key={index} size="sm" bold className={textClass}>
          {token.value}
        </Text>
      );
    case 'italic':
      return (
        <Text key={index} size="sm" className={`italic ${textClass}`}>
          {token.value}
        </Text>
      );
    case 'code':
      return (
        <Text key={index} size="sm" className={`bg-muted/50 px-1 rounded font-mono ${textClass}`}>
          {token.value}
        </Text>
      );
    case 'link': {
      const handlePress = () => {
        void Linking.openURL(token.url);
      };
      return Platform.OS === 'web' ? (
        // On web, spread href/target/rel so middle-click / cmd-click work naturally.
        <Text
          key={index}
          size="sm"
          className={`underline text-primary`}
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          {...({ href: token.url, target: '_blank', rel: 'noopener noreferrer' } as any)}
        >
          {token.label}
        </Text>
      ) : (
        <Text key={index} size="sm" className="underline text-primary" onPress={handlePress}>
          {token.label}
        </Text>
      );
    }
    case 'linebreak':
      return (
        <Text key={index} size="sm" className={textClass}>
          {'\n'}
        </Text>
      );
    default:
      return null;
  }
}

/** Renders one block of inline text, splitting it into inline tokens. */
function InlineBlock({ value, textClass }: { value: string; textClass: string }) {
  if (!value.trim()) return null;
  const inlineTokens = tokenizeInline(value);
  return (
    <Text size="sm" className={textClass}>
      {inlineTokens.map((t, i) => (
        <InlineSpan key={i} token={t} textClass={textClass} index={i} />
      ))}
    </Text>
  );
}

// Splits message text into block + inline segments and renders markdown.
function ChatText({ text, role }: { text: string; role: 'user' | 'assistant' }) {
  const textClass = role === 'user' ? 'text-primary-foreground' : 'text-foreground';
  const blocks = tokenizeBlocks(text);

  return (
    <VStack space="xs">
      {blocks.map((block, i) => {
        if (block.kind === 'image') {
          return (
            <Image
              key={i}
              source={{ uri: block.src }}
              resizeMode="contain"
              style={{ width: 240, height: 180, borderRadius: 8 }}
            />
          );
        }
        if (block.kind === 'code_block') {
          // Inside a user bubble keep plain foreground — gold-on-gold
          // is acceptable for the rare case of user-submitted code.
          // Syntax coloring is skipped for user role to avoid contrast issues
          // with the gold bubble background.
          const syntaxTokens = role === 'assistant' ? tokenizeCode(block.code, block.lang) : null;
          return (
            <ScrollView
              key={i}
              horizontal
              showsHorizontalScrollIndicator
              className="bg-muted rounded-lg"
              contentContainerStyle={{ padding: 10 }}
            >
              {syntaxTokens ? (
                <Text size="xs" className="font-mono">
                  {syntaxTokens.map((tok, ti) => {
                    // Map token kind to a Tailwind color class.
                    // kw      → text-primary        (gold — matches theme accent)
                    // str     → text-status-success  (green — string literals)
                    // num     → text-status-running  (blue — numeric literals)
                    // comment → text-muted-foreground italic
                    // punct   → text-foreground/60   (slightly dimmed punctuation)
                    // text    → text-foreground       (default)
                    let cls = 'text-foreground';
                    if (tok.kind === 'kw') cls = 'text-primary';
                    else if (tok.kind === 'str') cls = 'text-status-success';
                    else if (tok.kind === 'num') cls = 'text-status-running';
                    else if (tok.kind === 'comment') cls = 'text-muted-foreground italic';
                    else if (tok.kind === 'punct') cls = 'text-foreground/60';
                    return (
                      <Text key={ti} size="xs" className={`font-mono ${cls}`}>
                        {tok.value}
                      </Text>
                    );
                  })}
                </Text>
              ) : (
                <Text size="xs" className="font-mono text-foreground">
                  {block.code}
                </Text>
              )}
            </ScrollView>
          );
        }
        // inline_text block
        return <InlineBlock key={i} value={block.value} textClass={textClass} />;
      })}
    </VStack>
  );
}

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
        <VStack space="xs" className="items-start">
          <Box className={bubbleStyle({ role: 'assistant' })}>
            <ChatText text={event.text} role="assistant" />
          </Box>
        </VStack>
      );

    case 'assistant_delta':
      return (
        <VStack space="xs" className="items-start">
          <Box className="rounded-2xl rounded-bl-sm px-4 py-3 max-w-[78%] bg-card border border-primary/30 shadow-xs">
            <ChatText text={event.text} role="assistant" />
          </Box>
        </VStack>
      );

    case 'user_message':
      return (
        <VStack space="xs" className="items-end">
          <Box className={bubbleStyle({ role: 'user' })}>
            <ChatText text={event.text} role="user" />
          </Box>
        </VStack>
      );

    case 'error':
      return (
        <Box className={eventCardStyle({ kind: 'error' })}>
          <VStack space="xs">
            <Text size="2xs" bold className="uppercase text-destructive">
              Error
            </Text>
            <Text size="xs" className="text-destructive font-mono">
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

    case 'status':
      // The header already shows current run status. Rendering every internal
      // status transition in the chat makes the conversation noisy and hard to
      // scan, especially on retried/resumed runs.
      return null;

    case 'turn_complete':
      return (
        <Box className="items-center py-0.5">
          <Text size="2xs" className="text-muted-foreground bg-muted px-2 py-1 rounded-full">
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

function WorkProgressCard({
  run,
  events,
  onCancel,
  onRefresh,
}: {
  run: RunDetail;
  events: RunEventRow[];
  onCancel: () => void;
  onRefresh: () => void;
}) {
  const [tick, setTick] = useState(0);

  const last = [...events].reverse().find((row) => row.event.type !== 'status')?.event;
  const startedAt = new Date(run.lastEventAt ?? run.updatedAt ?? run.createdAt).getTime();
  const elapsedSeconds = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
  const elapsed =
    elapsedSeconds < 60 ? `${elapsedSeconds}s` : `${Math.floor(elapsedSeconds / 60)}m`;
  const label = (() => {
    if (!last) return 'Preparing your sprite';
    if (last.type === 'user_message') return 'Sending your prompt to the sprite';
    if (last.type === 'tool_use') return `Running ${last.name}`;
    if (last.type === 'tool_result') return 'Reviewing tool output';
    if (last.type === 'assistant_delta') return 'Writing a response';
    if (last.type === 'permission_requested') return 'Waiting for your approval';
    return run.status === 'starting' ? 'Starting the workspace' : 'Thinking';
  })();
  const detail =
    elapsedSeconds > 45
      ? 'This is taking longer than usual. We are checking the worker automatically.'
      : 'Most chats start within a few seconds. First wake-up can take longer.';

  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 1000);
    return () => clearInterval(timer);
  }, []);

  return (
    <Box className="rounded-2xl border border-border bg-card px-4 py-4 shadow-xs self-start w-full max-w-[560px]">
      <VStack space="md">
        <HStack space="sm" className="items-start">
          <Spinner size="small" />
          <VStack space="xs" className="flex-1">
            <HStack space="sm" className="items-center flex-wrap">
              <Text size="sm" bold className="text-foreground">
                {label}
                {'.'.repeat((tick % 3) + 1)}
              </Text>
              <Text size="2xs" className="text-muted-foreground">
                {elapsed}
              </Text>
            </HStack>
            <Text size="xs" className="text-muted-foreground">
              {detail}
            </Text>
          </VStack>
        </HStack>
        <HStack space="sm" className="items-center">
          <Button variant="outline" size="sm" onPress={onRefresh}>
            <ButtonText>Check status</ButtonText>
          </Button>
          <Button variant="link" size="sm" onPress={onCancel}>
            <ButtonText>Cancel</ButtonText>
          </Button>
        </HStack>
      </VStack>
    </Box>
  );
}

export default function RunDetailScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const [message, setMessage] = useState('');
  const [attachedImages, setAttachedImages] = useState<string[]>([]);
  const [pendingMessages, setPendingMessages] = useState<Array<{ id: string; text: string }>>([]);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [activeTab, setActiveTab] = useState<ActiveTab>('timeline');
  const flatListRef = useRef<RNFlatListType<RunEventRow>>(null);
  const webScrollRef = useRef<ScrollView | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const toast = useToast();

  const { token, baseUrl } = useAppConfig();

  const { data: run, isLoading, refetch: refetchRun } = useRun(id);
  useRunEventStream(id);

  const sendMsg = useSendMessage(id);
  const cancelRun = useCancelRun(id);
  const resolvePermission = useResolvePermission(id);
  const spriteId = run?.spriteId ?? '';
  const { data: sprite } = useSprite(spriteId);
  const suspendSprite = useSuspendSprite(spriteId);
  const resumeSprite = useResumeSprite(spriteId);

  const isBusy = run?.status === 'running' || run?.status === 'starting';
  const isActive = isBusy || run?.status === 'awaiting_input';
  // A follow-up message is what wakes an old suspended sprite and resumes the
  // saved CLI session. The sidecar only rejects messages while a turn is busy.
  const canSend = !!run && !isBusy;
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
    if (!canSend || sendMsg.isPending || (!text && attachedImages.length === 0)) return;

    // Upload each attached image to the server first, then compose the message
    // with server-side URLs so base64 data never lands in the SQLite event store.
    let imageUrls: string[] = [];
    if (attachedImages.length > 0) {
      try {
        imageUrls = await Promise.all(
          attachedImages.map(async (src) => {
            // On web, attachedImages holds data: URIs produced by FileReader.
            // Convert back to a Blob so we can POST multipart.
            const res = await fetch(src);
            const blob = await res.blob();
            const result = await uploadAttachment(baseUrl, token, id ?? '', blob);
            // Return an absolute URL so the markdown works from any origin.
            return `${baseUrl}${result.url}`;
          }),
        );
      } catch (e) {
        showError(e instanceof Error ? e.message : 'Failed to upload image(s).');
        return;
      }
    }

    const imageMd = imageUrls.map((url) => `![image](${url})`).join('\n');
    const composed = [text, imageMd].filter(Boolean).join('\n');
    const pendingId = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
    setPendingMessages((prev) => [...prev, { id: pendingId, text: composed }]);
    setMessage('');
    setAttachedImages([]);
    try {
      await sendMsg.mutateAsync(composed);
    } catch (e) {
      setPendingMessages((prev) => prev.filter((p) => p.id !== pendingId));
      showError(e instanceof Error ? e.message : 'Failed to send message.');
    }
  }

  function handleAttachClick() {
    if (Platform.OS === 'web') {
      fileInputRef.current?.click();
    } else {
      showError('Image attachments are web-only for now.');
    }
  }

  function handleComposerKeyPress(e: unknown) {
    if (Platform.OS !== 'web') return;
    const event = e as {
      key?: string;
      shiftKey?: boolean;
      preventDefault?: () => void;
      nativeEvent?: { key?: string; shiftKey?: boolean; isComposing?: boolean };
    };
    const key = event.key ?? event.nativeEvent?.key;
    const shiftKey = event.shiftKey ?? event.nativeEvent?.shiftKey ?? false;
    if (key !== 'Enter' || shiftKey || event.nativeEvent?.isComposing) return;
    event.preventDefault?.();
    void handleSend();
  }

  function handleFilesPicked(files: FileList | null) {
    if (!files || files.length === 0) return;
    const readers = Array.from(files)
      .filter((f) => f.type.startsWith('image/'))
      .slice(0, 4 - attachedImages.length)
      .map(
        (f) =>
          new Promise<string>((resolve, reject) => {
            const r = new FileReader();
            r.onload = () => resolve(String(r.result));
            r.onerror = reject;
            r.readAsDataURL(f);
          }),
      );
    void Promise.all(readers).then((urls) => {
      setAttachedImages((prev) => [...prev, ...urls].slice(0, 4));
    });
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

  // Hooks must be called unconditionally — call before any early return.
  const pulseStyle = usePulse(run?.status === 'running');
  const insets = useSafeAreaInsets();
  const visibleEvents = useMemo(() => {
    if (!run) return [];
    const confirmedTexts = new Set(
      run.events
        .filter((eventRow) => eventRow.event.type === 'user_message')
        .map((eventRow) => (eventRow.event.type === 'user_message' ? eventRow.event.text : '')),
    );
    const pendingRows: RunEventRow[] = pendingMessages
      .filter((pending) => !confirmedTexts.has(pending.text))
      .map((pending, index) => ({
        runId: run.id,
        seq: Date.now() + index,
        event: { type: 'user_message', text: pending.text, at: new Date().toISOString() },
        createdAt: new Date().toISOString(),
      }));
    const rows: RunEventRow[] = [];
    let deltaText = '';
    let deltaStart: RunEventRow | null = null;

    const flushDelta = () => {
      if (!deltaStart || !deltaText) return;
      rows.push({
        ...deltaStart,
        event: { type: 'assistant_delta', text: deltaText, at: deltaStart.event.at },
      });
      deltaText = '';
      deltaStart = null;
    };

    for (const row of run.events) {
      if (row.event.type === 'assistant_delta') {
        deltaStart ??= row;
        deltaText += row.event.text;
        continue;
      }
      if (row.event.type === 'assistant_text' && deltaText) {
        deltaText = '';
        deltaStart = null;
        rows.push(row);
        continue;
      }
      flushDelta();
      rows.push(row);
    }
    flushDelta();

    return [...rows, ...pendingRows];
  }, [pendingMessages, run]);

  if (isLoading || !run) {
    return (
      <Box className="flex-1 items-center justify-center bg-background">
        <Spinner size="large" />
      </Box>
    );
  }

  const color = statusColor(run.status);
  const canSubmit =
    canSend && !sendMsg.isPending && (!!message.trim() || attachedImages.length > 0);
  const emptyEventsText =
    run.status === 'failed'
      ? 'This chat failed before emitting timeline events. Send a message to wake the sprite and try again.'
      : 'No events yet. The run is starting…';
  const showWorkLoader = run.status === 'running' || run.status === 'starting';

  // Shell tab is only rendered on web; on native it shows the placeholder.
  const showShellTab = Platform.OS === 'web' && !!run.spriteId;

  return (
    <Box className="flex-1 bg-background">
      {/* Header info — padTop with safe-area inset so it clears the Stack
          header on mobile without competing with it. On web/desktop the Stack
          header is hidden so insets.top is 0 and there is no extra padding. */}
      <Box
        className="bg-card border-b border-border px-4 py-2 md:px-6 md:py-3"
        style={{ paddingTop: insets.top > 0 ? insets.top : undefined }}
      >
        <VStack space="xs">
          <HStack space="sm" className="items-center">
            <CrossfadePill contentKey={run.status}>
              <HStack space="xs" className="items-center">
                <Animated.View
                  style={pulseStyle}
                  className={`w-2 h-2 rounded-full ${color.dotClass}`}
                />
                <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
                  {color.label}
                </Text>
              </HStack>
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
              <Text size="sm" bold className="text-foreground" numberOfLines={1}>
                {run.title}
              </Text>
              <Text size="xs" className="text-muted-foreground" numberOfLines={1}>
                {run.prompt}
              </Text>
            </VStack>
          ) : (
            <Text size="sm" className="text-foreground" numberOfLines={2}>
              {run.prompt}
            </Text>
          )}
          {run.totalCostUsd > 0 ? (
            <Text size="xs" className="text-muted-foreground">
              {formatCost(run.totalCostUsd)} · {run.totalInputTokens}↑ {run.totalOutputTokens}↓
            </Text>
          ) : null}

          {/* Tab segment: Timeline | Shell */}
          <HStack space="sm" className="pt-0.5">
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
            {Platform.OS === 'web' ? (
              <ScrollView
                ref={webScrollRef}
                className="flex-1"
                contentContainerStyle={{
                  padding: 16,
                  gap: 12,
                  maxWidth: 960,
                  width: '100%',
                  alignSelf: 'center',
                }}
                onContentSizeChange={() => webScrollRef.current?.scrollToEnd({ animated: true })}
              >
                {visibleEvents.length > 0 ? (
                  visibleEvents.map((eventRow) => (
                    <EventCard key={eventRow.seq} row={eventRow} onResolve={handleResolve} />
                  ))
                ) : (
                  <Text size="sm" className="text-muted-foreground text-center pt-10">
                    {emptyEventsText}
                  </Text>
                )}
                {showWorkLoader ? (
                  <WorkProgressCard
                    run={run}
                    events={visibleEvents}
                    onCancel={() => setConfirmCancel(true)}
                    onRefresh={() => void refetchRun()}
                  />
                ) : null}
              </ScrollView>
            ) : (
              <FlatList
                ref={flatListRef}
                data={visibleEvents}
                renderItem={renderItem}
                keyExtractor={keyExtractor}
                contentContainerStyle={{ padding: 16, gap: 12 }}
                onContentSizeChange={() => flatListRef.current?.scrollToEnd({ animated: true })}
                ListEmptyComponent={
                  <Text size="sm" className="text-muted-foreground text-center pt-10">
                    {emptyEventsText}
                  </Text>
                }
                ListFooterComponent={
                  showWorkLoader ? (
                    <WorkProgressCard
                      run={run}
                      events={visibleEvents}
                      onCancel={() => setConfirmCancel(true)}
                      onRefresh={() => void refetchRun()}
                    />
                  ) : null
                }
              />
            )}

            {/* Composer */}
            <VStack className="p-3 md:p-4 bg-card border-t border-border shadow-xs" space="sm">
              <VStack
                className="w-full max-w-[960px] self-center rounded-3xl border border-border bg-background px-3 py-3 shadow-xs"
                space="sm"
              >
                {attachedImages.length > 0 ? (
                  <HStack space="xs" className="flex-wrap">
                    {attachedImages.map((src, i) => (
                      <Box key={i} className="relative">
                        <Image
                          source={{ uri: src }}
                          resizeMode="cover"
                          style={{ width: 56, height: 56, borderRadius: 10 }}
                        />
                        <Pressable
                          onPress={() =>
                            setAttachedImages((prev) => prev.filter((_, j) => j !== i))
                          }
                          className="absolute -top-1 -right-1 w-5 h-5 rounded-full bg-destructive items-center justify-center"
                        >
                          <Text size="2xs" bold className="text-destructive-foreground">
                            ×
                          </Text>
                        </Pressable>
                      </Box>
                    ))}
                  </HStack>
                ) : null}

                <Input className="border-0 bg-transparent min-h-12">
                  <InputField
                    value={message}
                    onChangeText={setMessage}
                    multiline
                    textAlignVertical="center"
                    placeholder={isBusy ? 'Waiting for agent…' : 'Send a message…'}
                    editable={canSend}
                    className="min-h-12 max-h-36 px-1 py-2 text-base"
                    style={{ lineHeight: 22 }}
                    onKeyPress={handleComposerKeyPress}
                  />
                </Input>

                <HStack className="items-center justify-between">
                  {Platform.OS === 'web' ? (
                    <Pressable
                      onPress={handleAttachClick}
                      disabled={!canSend || attachedImages.length >= 4}
                      className="h-10 w-10 rounded-full border border-border items-center justify-center data-[hover=true]:bg-accent disabled:opacity-50"
                      accessibilityLabel="Attach image"
                    >
                      <Feather name="paperclip" size={20} color="#737373" />
                    </Pressable>
                  ) : (
                    <Box />
                  )}
                  <Button
                    variant="default"
                    className="h-10 w-10 rounded-full p-0 items-center justify-center"
                    isDisabled={!canSubmit}
                    onPress={() => void handleSend()}
                    accessibilityLabel="Send message"
                  >
                    {sendMsg.isPending ? (
                      <ButtonSpinner />
                    ) : (
                      <Feather name="send" size={18} color="#171717" />
                    )}
                  </Button>
                </HStack>
              </VStack>
              {Platform.OS === 'web' ? (
                <input
                  ref={fileInputRef}
                  type="file"
                  accept="image/*"
                  multiple
                  style={{ display: 'none' }}
                  onChange={(e: { target: { files: FileList | null; value: string } }) => {
                    handleFilesPicked(e.target.files);
                    e.target.value = '';
                  }}
                />
              ) : null}
            </VStack>
          </>
        )}
      </TabCrossfade>

      <AlertDialog isOpen={confirmCancel} onClose={() => setConfirmCancel(false)}>
        <AlertDialogBackdrop />
        <AlertDialogContent>
          <AlertDialogHeader>
            <Heading size="md" bold className="text-foreground">
              Cancel chat?
            </Heading>
          </AlertDialogHeader>
          <AlertDialogBody>
            <Text size="sm" className="text-muted-foreground">
              Are you sure you want to cancel this chat? Any in-flight work will be lost.
            </Text>
          </AlertDialogBody>
          <AlertDialogFooter>
            <Button variant="outline" size="sm" onPress={() => setConfirmCancel(false)}>
              <ButtonText>Keep running</ButtonText>
            </Button>
            <Button variant="destructive" size="sm" onPress={() => void performCancel()}>
              <ButtonText>Cancel chat</ButtonText>
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Box>
  );
}
