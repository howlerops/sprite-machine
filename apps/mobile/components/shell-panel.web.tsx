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

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Button, ButtonText } from '@/components/ui/button';
import { Badge, BadgeText } from '@/components/ui/badge';

import { clearEmail } from '../src/storage';
import { useAppConfig } from '../src/config-store';
import type { SpriteRow } from '../src/api';

type PaneState = 'connecting' | 'open' | 'suspended' | 'cold' | 'disconnected' | 'auth_error';

interface ShellPanelProps {
  spriteId: string;
  runId: string;
  sprite?: SpriteRow | null;
}

function buildWsUrl(spriteId: string): string {
  if (typeof window === 'undefined') return '';
  const origin = window.location.origin;
  const wsOrigin = origin.replace(/^http/, 'ws');
  return `${wsOrigin}/sprites/${encodeURIComponent(spriteId)}/exec?tty=true`;
}

export default function ShellPanel({ spriteId, runId: _runId, sprite }: ShellPanelProps) {
  const router = useRouter();
  const { setEmail: setEmailCtx } = useAppConfig();
  const [paneState, setPaneState] = useState<PaneState>('connecting');
  const [reconnectKey, setReconnectKey] = useState(0);
  const termRef = useRef<TerminalHandle>(null);
  const wsRef = useRef<WebSocket | null>(null);

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

    ws.onopen = () => {
      setPaneState('open');
    };

    ws.onmessage = (evt: MessageEvent) => {
      const term = termRef.current;
      if (!term) return;
      if (evt.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(evt.data));
      } else if (typeof evt.data === 'string') {
        term.write(evt.data);
      }
    };

    ws.onclose = (evt: CloseEvent) => {
      wsRef.current = null;
      if (evt.code === 4001) setPaneState('suspended');
      else if (evt.code === 4002) setPaneState('cold');
      else if (evt.code === 4401 || evt.code === 1008) setPaneState('auth_error');
      else setPaneState('disconnected');
    };

    ws.onerror = () => {
      setPaneState('disconnected');
    };

    return () => {
      ws.close();
      wsRef.current = null;
    };
  }, [spriteId, reconnectKey, isSuspended]);

  async function handleAuthError() {
    await clearEmail();
    setEmailCtx(null);
    router.replace('/login');
  }

  function handleReconnect() {
    setReconnectKey((k) => k + 1);
    setPaneState('connecting');
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
          <BadgeText>{spriteStatus}</BadgeText>
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
              ws.send(data);
            }
          }}
          style={{ width: '100%', height: '100%' }}
        />
      </Box>
    </Box>
  );
}
