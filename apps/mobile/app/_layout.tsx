import { useState, useEffect, useCallback, useMemo } from 'react';
import { Platform, useColorScheme, useWindowDimensions } from 'react-native';
import { Stack, useRouter, useSegments } from 'expo-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { StatusBar } from 'expo-status-bar';

import { GluestackUIProvider } from '@/components/ui/gluestack-ui-provider';
import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { ScrollView } from '@/components/ui/scroll-view';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Button, ButtonText } from '@/components/ui/button';
import '@/global.css';

import { AppConfigContext, useAppConfig } from '../src/config-store';
import {
  getToken,
  getBaseUrl,
  getEmail,
  getThemeOverride,
  setThemeOverride as persistThemeOverride,
  DEFAULT_BASE_URL,
  type ThemeOverride,
} from '../src/storage';
import { usePushRegistration } from '../src/push';
import { useRuns } from '../src/hooks/useRuns';
import { relativeTime, truncate, statusColor } from '../src/utils';
import type { RunRow } from '../src/api';

const LG_BREAKPOINT = 1024;

const THEME_OVERRIDE_KEY = 'sprite_machine_theme_override';

/**
 * Synchronously read the persisted theme override on web so the very first
 * render uses the correct mode and we never paint a flash. On native we
 * return null and rely on async hydration (no flash there because the
 * provider mounts a blank canvas while loading).
 */
function readThemeOverrideSync(): ThemeOverride | null {
  if (Platform.OS !== 'web') return null;
  if (typeof window === 'undefined' || !window.localStorage) return null;
  try {
    const v = window.localStorage.getItem(THEME_OVERRIDE_KEY);
    if (v === 'system' || v === 'light' || v === 'dark') return v;
  } catch {
    // localStorage may be unavailable (SSR, privacy mode); fall back to async.
  }
  return null;
}

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      staleTime: 5_000,
    },
  },
});

type Hydrated = {
  token: string | null;
  baseUrl: string;
  email: string | null;
  themeOverride: ThemeOverride;
};

function PushSetup() {
  usePushRegistration();
  return null;
}

function RouteGuard({ children }: { children: React.ReactNode }) {
  const segments = useSegments();
  const router = useRouter();
  const { token, email } = useAppConfig();

  useEffect(() => {
    const inSettings = segments.includes('settings');
    const inLogin = segments.includes('login');

    if (Platform.OS === 'web') {
      // Web: guard on email (cookie session). If no email, redirect to /login
      // unless already there.
      if (!email && !inLogin) {
        router.replace('/login');
      }
    } else {
      // Native iOS: keep original bearer-token guard.
      if (!token && !inSettings) {
        router.replace('/(tabs)/settings');
      }
    }
  }, [token, email, segments, router]);

  return <>{children}</>;
}

function RailRunItem({
  run,
  isActive,
  onPress,
}: {
  run: RunRow;
  isActive: boolean;
  onPress: () => void;
}) {
  const color = statusColor(run.status);
  return (
    <Pressable
      onPress={onPress}
      className={
        isActive
          ? 'p-3 border-b border-border bg-accent'
          : 'p-3 border-b border-border data-[hover=true]:bg-accent data-[active=true]:bg-accent/80'
      }
    >
      <VStack space="xs">
        <HStack space="sm" className="items-center">
          <Box className={`px-2 py-0.5 rounded-full ${color.bgClass}`}>
            <Text size="2xs" bold className={`uppercase ${color.textClass}`}>
              {color.label}
            </Text>
          </Box>
          <Text size="2xs" className="text-muted-foreground ml-auto">
            {relativeTime(run.updatedAt)}
          </Text>
        </HStack>
        <Text size="xs" className="text-foreground" numberOfLines={2}>
          {truncate(run.prompt, 80)}
        </Text>
      </VStack>
    </Pressable>
  );
}

function DesktopRail() {
  const router = useRouter();
  const segments = useSegments();
  const { data } = useRuns();
  const runs: RunRow[] = data?.runs ?? [];

  const inSettings = segments.includes('settings');
  const runIdx = segments.indexOf('run');
  const activeRunId =
    runIdx >= 0 && runIdx + 1 < segments.length ? segments[runIdx + 1] : null;
  const inRuns = !inSettings && !activeRunId;

  return (
    <Box className="w-72 lg:w-72 xl:w-80 bg-card border-r border-border">
      <VStack className="h-full">
        <Box className="px-4 py-4 border-b border-border">
          <Heading size="md" bold className="text-foreground">
            sprite-machine
          </Heading>
        </Box>

        <Box className="px-3 py-3 border-b border-border">
          <Button
            variant="default"
            size="sm"
            onPress={() => router.push('/new')}
            className="w-full"
          >
            <ButtonText>+ New run</ButtonText>
          </Button>
        </Box>

        <Box className="flex-1">
          <ScrollView className="flex-1">
            {runs.length === 0 ? (
              <Box className="p-4">
                <Text size="xs" className="text-muted-foreground">
                  No runs yet.
                </Text>
              </Box>
            ) : (
              runs.map((run) => (
                <RailRunItem
                  key={run.id}
                  run={run}
                  isActive={activeRunId === run.id}
                  onPress={() => router.push(`/run/${run.id}`)}
                />
              ))
            )}
          </ScrollView>
        </Box>

        <Box className="border-t border-border">
          <Pressable
            onPress={() => router.push('/(tabs)')}
            className={
              inRuns ? 'px-4 py-3 bg-accent' : 'px-4 py-3 data-[hover=true]:bg-accent'
            }
          >
            <Text size="sm" className="text-foreground">
              Runs
            </Text>
          </Pressable>
          <Pressable
            onPress={() => router.push('/(tabs)/settings')}
            className={
              inSettings
                ? 'px-4 py-3 bg-accent'
                : 'px-4 py-3 data-[hover=true]:bg-accent'
            }
          >
            <Text size="sm" className="text-foreground">
              Settings
            </Text>
          </Pressable>
        </Box>
      </VStack>
    </Box>
  );
}

function ResponsiveShell({ children }: { children: React.ReactNode }) {
  const { width } = useWindowDimensions();
  const segments = useSegments();
  const isDesktop = width >= LG_BREAKPOINT;
  const onLogin = segments.includes('login');

  if (!isDesktop || onLogin) return <>{children}</>;

  return (
    <Box className="flex-1 flex-row bg-background">
      <DesktopRail />
      <Box className="flex-1 bg-background">{children}</Box>
    </Box>
  );
}

export default function RootLayout() {
  const systemScheme = useColorScheme();
  const [initialThemeOverride] = useState<ThemeOverride | null>(() =>
    readThemeOverrideSync(),
  );
  const [hydrated, setHydrated] = useState<Hydrated | null>(null);

  useEffect(() => {
    void (async () => {
      const [t, u, em, theme] = await Promise.all([
        getToken(),
        getBaseUrl(),
        getEmail(),
        getThemeOverride(),
      ]);
      setHydrated({ token: t, baseUrl: u, email: em, themeOverride: theme });
    })();
  }, []);

  const setToken = useCallback((t: string | null) => {
    setHydrated((prev) => (prev ? { ...prev, token: t } : prev));
  }, []);
  const setBaseUrl = useCallback((u: string) => {
    setHydrated((prev) => (prev ? { ...prev, baseUrl: u } : prev));
  }, []);
  const setEmail = useCallback((em: string | null) => {
    setHydrated((prev) => (prev ? { ...prev, email: em } : prev));
  }, []);
  const setThemeOverride = useCallback((value: ThemeOverride) => {
    setHydrated((prev) => (prev ? { ...prev, themeOverride: value } : prev));
    void persistThemeOverride(value);
  }, []);

  const resolved: 'light' | 'dark' = useMemo(() => {
    const override =
      hydrated?.themeOverride ?? initialThemeOverride ?? 'system';
    if (override === 'system') return systemScheme === 'dark' ? 'dark' : 'light';
    return override;
  }, [hydrated?.themeOverride, initialThemeOverride, systemScheme]);

  const ctx = useMemo(
    () => ({
      token: hydrated?.token ?? null,
      baseUrl: hydrated?.baseUrl ?? DEFAULT_BASE_URL,
      email: hydrated?.email ?? null,
      themeOverride: hydrated?.themeOverride ?? 'system',
      setToken,
      setBaseUrl,
      setEmail,
      setThemeOverride,
    }),
    [hydrated, setToken, setBaseUrl, setEmail, setThemeOverride],
  );

  if (!hydrated) {
    return <GluestackUIProvider mode={resolved} />;
  }

  return (
    <GluestackUIProvider mode={resolved}>
      <QueryClientProvider client={queryClient}>
        <AppConfigContext.Provider value={ctx}>
          {hydrated.token ? <PushSetup /> : null}
          <RouteGuard>
            <StatusBar style="auto" />
            <ResponsiveShell>
              <Stack>
                <Stack.Screen name="(tabs)" options={{ headerShown: false }} />
                <Stack.Screen
                  name="run/[id]"
                  options={{ title: 'Run', headerBackTitle: 'Runs' }}
                />
                <Stack.Screen
                  name="new"
                  options={{ title: 'New Run', presentation: 'modal', headerBackTitle: 'Cancel' }}
                />
                <Stack.Screen
                  name="login"
                  options={{ headerShown: false }}
                />
              </Stack>
            </ResponsiveShell>
          </RouteGuard>
        </AppConfigContext.Provider>
      </QueryClientProvider>
    </GluestackUIProvider>
  );
}
