/**
 * Web-only shell panel. The whole module is only resolved when bundling for
 * web (Metro picks `.web.tsx` via its platform-extension resolver), so a
 * top-level static import of `@wterm/react` is safe — it never lands in the
 * native bundle. The WebSocket URL is built from the current
 * window.location.origin so it works in both local dev and prod.
 *
 * @wterm/react Terminal is a controlled component: it doesn't manage its own
 * WS connection. We open the WS here, pipe incoming binary frames into
 * terminal.write(), and send keystrokes back via ws.send().
 */
import { useEffect, useRef, useState } from 'react';
import { useRouter } from 'expo-router';
import { Terminal, type TerminalHandle } from '@wterm/react';
import { useReduceMotion } from '../src/animations';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Button, ButtonText } from '@/components/ui/button';
import { Badge, BadgeText } from '@/components/ui/badge';

import { clearEmail } from '../src/storage';
import { useAppConfig } from '../src/config-store';
import { useResumeSprite } from '../src/hooks/useSprites';
import type { SpriteRow } from '../src/api';

type PaneState = 'connecting' | 'open' | 'suspended' | 'cold' | 'disconnected' | 'auth_error' | 'sprite_unavailable';

interface ShellPanelProps {
  spriteId: string;
  runId: string;
  sprite?: SpriteRow | null;
}

function buildWsUrl(spriteId: string): string {
  if (typeof window === 'undefined') return '';
  const origin = window.location.origin;
  const wsOrigin = origin.replace(/^http/, 'ws');
  const params = new URLSearchParams();
  // Start an interactive login shell in the sprite workspace when present. The
  // Sprites exec API accepts repeated cmd params as argv.
  params.append('cmd', 'bash');
  params.append('cmd', '-lc');
  params.append('cmd', 'cd "${SPRITE_AGENT_WORKDIR:-$PWD}" 2>/dev/null || cd "$HOME"; exec bash -l');
  params.set('tty', 'true');
  return `${wsOrigin}/sprites/${encodeURIComponent(spriteId)}/exec?${params.toString()}`;
}

function sendStdinFrame(ws: WebSocket, data: string): void {
  const payload = new TextEncoder().encode(data);
  const frame = new Uint8Array(payload.length + 1);
  frame[0] = 0; // stdin stream id expected by Sprites exec
  frame.set(payload, 1);
  ws.send(frame);
}

export default function ShellPanel({ spriteId, runId: _runId, sprite }: ShellPanelProps) {
  const router = useRouter();
  const { setEmail: setEmailCtx } = useAppConfig();
  const [paneState, setPaneState] = useState<PaneState>('connecting');
  const [reconnectKey, setReconnectKey] = useState(0);
  const termRef = useRef<TerminalHandle>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const reduceMotion = useReduceMotion();
  const resumeSprite = useResumeSprite(spriteId);
  // Track whether the very first connect for this mount has happened. Only the
  // first ever stream gets the typewriter reveal; reconnects skip it.
  const firstConnectDoneRef = useRef(false);
  const typewriterBufferRef = useRef<Uint8Array | null>(null);
  const typewriterIdxRef = useRef(0);
  const typewriterTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const spriteStatus = sprite?.status ?? 'running';
  const isSuspended = spriteStatus === 'suspended';

  // Manage the WebSocket lifecycle tied to reconnectKey.
  useEffect(() => {
    if (isSuspended) return;

    const wsUrl = buildWsUrl(spriteId);
    if (!wsUrl) return;

    const ws = new WebSocket(wsUrl);
    wsRef.current = ws;
    ws.binaryType = 'arraybuffer';

    // Buffer until we've decided whether to typewriter-reveal or stream raw.
    let typewriterActive = !firstConnectDoneRef.current && !reduceMotion;
    let writtenBytes = 0;
    const TYPEWRITER_LIMIT = 30;
    const INTERVAL_MS = 18;

    const flushTypewriter = () => {
      const term = termRef.current;
      const buf = typewriterBufferRef.current;
      if (!term || !buf) return;
      const remaining = buf.subarray(typewriterIdxRef.current);
      if (remaining.length > 0) term.write(remaining);
      typewriterIdxRef.current = buf.length;
      typewriterActive = false;
      if (typewriterTimerRef.current) {
        clearInterval(typewriterTimerRef.current);
        typewriterTimerRef.current = null;
      }
    };

    ws.onopen = () => {
      setPaneState('open');
    };

    ws.onmessage = (evt: MessageEvent) => {
      const term = termRef.current;
      if (!term) return;

      let chunk: Uint8Array | null = null;
      if (evt.data instanceof ArrayBuffer) {
        const frame = new Uint8Array(evt.data);
        if (frame.length === 0) return;
        const streamId = frame[0];
        // Sprites exec binary protocol prefixes stdout/stderr with a stream id.
        // 1=stdout, 2=stderr, 3=exit, 4=eof. Only terminal output should be
        // written; strip the stream byte so control chars don't pollute wterm.
        if (streamId !== 1 && streamId !== 2) return;
        chunk = frame.subarray(1);
      } else if (typeof evt.data === 'string') {
        // The exec endpoint sends JSON session metadata before terminal output.
        // Do not render it in the terminal; it made the shell look unusable.
        try {
          const parsed = JSON.parse(evt.data) as { type?: string };
          if (parsed.type === 'session_info') return;
        } catch {
          // Non-JSON string output is valid terminal text.
        }
        chunk = new TextEncoder().encode(evt.data);
      }
      if (!chunk || chunk.length === 0) return;

      if (!typewriterActive) {
        term.write(chunk);
        return;
      }

      // Accumulate into the typewriter buffer; reveal one byte every 18ms
      // until we hit TYPEWRITER_LIMIT, then flush the rest and switch to
      // raw-stream mode.
      const prev = typewriterBufferRef.current;
      if (prev) {
        const next = new Uint8Array(prev.length + chunk.length);
        next.set(prev, 0);
        next.set(chunk, prev.length);
        typewriterBufferRef.current = next;
      } else {
        typewriterBufferRef.current = chunk;
      }

      if (!typewriterTimerRef.current) {
        firstConnectDoneRef.current = true;
        typewriterTimerRef.current = setInterval(() => {
          const buf = typewriterBufferRef.current;
          const t = termRef.current;
          if (!buf || !t) return;
          if (writtenBytes >= TYPEWRITER_LIMIT) {
            flushTypewriter();
            return;
          }
          if (typewriterIdxRef.current < buf.length) {
            t.write(buf.subarray(typewriterIdxRef.current, typewriterIdxRef.current + 1));
            typewriterIdxRef.current += 1;
            writtenBytes += 1;
          }
        }, INTERVAL_MS);
      }
    };

    ws.onclose = (evt: CloseEvent) => {
      wsRef.current = null;
      if (evt.code === 4001) setPaneState('suspended');
      else if (evt.code === 4002) setPaneState('cold');
      else if (evt.code === 4003) setPaneState('sprite_unavailable');
      else if (evt.code === 4401 || evt.code === 1008) setPaneState('auth_error');
      else setPaneState('disconnected');
    };

    ws.onerror = () => {
      setPaneState('disconnected');
    };

    return () => {
      if (typewriterTimerRef.current) {
        clearInterval(typewriterTimerRef.current);
        typewriterTimerRef.current = null;
      }
      typewriterBufferRef.current = null;
      typewriterIdxRef.current = 0;
      ws.close();
      wsRef.current = null;
    };
  }, [spriteId, reconnectKey, isSuspended, reduceMotion]);

  async function handleAuthError() {
    await clearEmail();
    setEmailCtx(null);
    router.replace('/login');
  }

  function handleReconnect() {
    setReconnectKey((k) => k + 1);
    setPaneState('connecting');
  }

  async function handleWakeAndReconnect() {
    setPaneState('connecting');
    try {
      await resumeSprite.mutateAsync();
    } finally {
      setReconnectKey((k) => k + 1);
    }
  }

  // Show suspended state if sprite is suspended.
  if (isSuspended) {
    return (
      <Box className="flex-1 items-center justify-center bg-background p-6">
        <VStack space="md" className="max-w-sm items-center">
          <Heading size="md" className="text-foreground text-center">
            Sprite is suspended
          </Heading>
          <Text size="sm" className="text-muted-foreground text-center">
            Resume to attach a shell.
          </Text>
        </VStack>
      </Box>
    );
  }

  if (paneState === 'sprite_unavailable') {
    return (
      <Box className="flex-1 items-center justify-center bg-background p-6">
        <VStack space="md" className="max-w-sm items-center">
          <Heading size="md" className="text-foreground text-center">
            Sprite no longer available
          </Heading>
          <Text size="sm" className="text-muted-foreground text-center">
            The shell endpoint did not accept a connection. Try waking the sprite;
            if it still fails, start a new chat to provision a fresh session.
          </Text>
          <HStack space="sm">
            <Button
              variant="default"
              onPress={() => void handleWakeAndReconnect()}
              isDisabled={resumeSprite.isPending}
            >
              <ButtonText>{resumeSprite.isPending ? 'Waking…' : 'Wake sprite'}</ButtonText>
            </Button>
            <Button variant="outline" onPress={() => router.push('/new')}>
              <ButtonText>New chat</ButtonText>
            </Button>
          </HStack>
        </VStack>
      </Box>
    );
  }

  if (paneState === 'cold') {
    return (
      <Box className="flex-1 items-center justify-center bg-background p-6">
        <VStack space="md" className="max-w-sm items-center">
          <Heading size="md" className="text-foreground text-center">
            Sprite hasn&apos;t booted
          </Heading>
        </VStack>
      </Box>
    );
  }

  if (paneState === 'auth_error') {
    return (
      <Box className="flex-1 items-center justify-center bg-background p-6">
        <VStack space="md" className="max-w-sm items-center">
          <Heading size="md" className="text-foreground">
            Session expired
          </Heading>
          <Button variant="default" onPress={() => void handleAuthError()}>
            <ButtonText>Sign in again</ButtonText>
          </Button>
        </VStack>
      </Box>
    );
  }

  return (
    <Box className="flex-1 bg-background p-4 md:p-6 flex-col">
      {/* Toolbar */}
      <HStack
        space="sm"
        className="items-center px-2 py-1.5 border-b border-border bg-card mb-2 rounded-t-[10px]"
      >
        <Badge variant="default">
          <BadgeText>{paneState === 'connecting' ? 'connecting' : spriteStatus}</BadgeText>
        </Badge>
        <Text size="xs" className="font-mono text-muted-foreground">
          {spriteId}
        </Text>
        <HStack space="sm" className="ml-auto items-center">
          <Button variant="outline" size="sm" onPress={handleReconnect}>
            <ButtonText>Reconnect</ButtonText>
          </Button>
        </HStack>
      </HStack>

      {/* Disconnected banner */}
      {paneState === 'disconnected' ? (
        <Box className="px-3 py-2 bg-status-warning-muted mb-2">
          <HStack space="sm" className="items-center">
            <Text size="sm" className="text-status-warning-foreground flex-1">
              Connection lost.
            </Text>
            <Button variant="outline" size="sm" onPress={() => void handleWakeAndReconnect()}>
              <ButtonText>{resumeSprite.isPending ? 'Waking…' : 'Wake'}</ButtonText>
            </Button>
            <Button variant="outline" size="sm" onPress={handleReconnect}>
              <ButtonText>Reconnect</ButtonText>
            </Button>
          </HStack>
        </Box>
      ) : null}

      {/* Terminal */}
      <Box className="flex-1 rounded-[10px] border border-border overflow-hidden">
        <Terminal
          key={reconnectKey}
          ref={termRef}
          onData={(data: string) => {
            const ws = wsRef.current;
            if (ws && ws.readyState === WebSocket.OPEN) {
              sendStdinFrame(ws, data);
            }
          }}
          style={{ width: '100%', height: '100%' }}
        />
      </Box>
    </Box>
  );
}
