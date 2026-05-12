import { useState, useEffect, useCallback, useMemo } from 'react';
import { Platform, useColorScheme, useWindowDimensions } from 'react-native';
import { Stack, useRouter, useSegments } from 'expo-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { StatusBar } from 'expo-status-bar';
import { Feather } from '@expo/vector-icons';
import { useFonts } from 'expo-font';

import { GluestackUIProvider } from '@/components/ui/gluestack-ui-provider';
import { Box } from '@/components/ui/box';
import { Pressable } from '@/components/ui/pressable';
import '@/global.css';

import { AppConfigContext, useAppConfig } from '../src/config-store';
import { Sidebar } from '../components/sidebar';
import { WolfMark } from '../components/wolf-mark';
import { DrawerProvider, useDrawer } from '../src/drawer-context';
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

const LG_BREAKPOINT = 1024;

const THEME_OVERRIDE_KEY = 'howlerforge_theme_override';
const LEGACY_THEME_OVERRIDE_KEY = 'sprite_machine_theme_override';

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
    const v =
      window.localStorage.getItem(THEME_OVERRIDE_KEY) ??
      window.localStorage.getItem(LEGACY_THEME_OVERRIDE_KEY);
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
    const segs = segments as readonly string[];
    const inSettings = segs.includes('settings');
    const inLogin = segs.includes('login');

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

/**
 * Hamburger button used in Stack / Tabs headerLeft. Reads drawer state from
 * DrawerContext so both layout levels share the same toggle.
 */
function HamburgerButton() {
  const { width } = useWindowDimensions();
  const { setOpen } = useDrawer();
  const colorScheme = useColorScheme();
  const iconColor = colorScheme === 'dark' ? '#fafafa' : '#171717';
  if (width >= LG_BREAKPOINT) return null;
  return (
    <Box className="flex-row items-center gap-3 pl-1">
      <Pressable
        onPress={() => setOpen(true)}
        accessibilityLabel="Open navigation"
        style={{ paddingHorizontal: 8, paddingVertical: 4 }}
      >
        <Feather name="menu" size={24} color={iconColor} />
      </Pressable>
      <WolfMark size={24} />
    </Box>
  );
}

function ResponsiveShell({ children }: { children: React.ReactNode }) {
  const { width } = useWindowDimensions();
  const segments = useSegments();
  const isDesktop = width >= LG_BREAKPOINT;
  const onLogin = (segments as readonly string[]).includes('login');
  const { open: drawerOpen, setOpen: setDrawerOpen } = useDrawer();

  if (onLogin) return <>{children}</>;

  if (isDesktop) {
    return (
      <Box className="flex-1 flex-row bg-background">
        <Sidebar mode="desktop" />
        <Box className="flex-1 bg-background">{children}</Box>
      </Box>
    );
  }

  // Mobile: render screen content, then overlay the sidebar drawer so it
  // can slide over everything including the Stack header.
  return (
    <Box className="flex-1 bg-background">
      {children}
      <Sidebar mode="mobile-drawer" open={drawerOpen} onClose={() => setDrawerOpen(false)} />
    </Box>
  );
}

export default function RootLayout() {
  const systemScheme = useColorScheme();
  const [initialThemeOverride] = useState<ThemeOverride | null>(() => readThemeOverrideSync());
  const [hydrated, setHydrated] = useState<Hydrated | null>(null);

  // Bug 1: load the Feather icon font on web so icons render as glyphs instead
  // of squares. expo-font is a transitive dep; importing here makes TS happy
  // and ensures the font is requested before any icon renders.
  const [fontsLoaded] = useFonts(Feather.font);

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
    const override = hydrated?.themeOverride ?? initialThemeOverride ?? 'system';
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

  // Hold the splash until both storage hydration and font loading are done.
  if (!hydrated || !fontsLoaded) {
    return <GluestackUIProvider mode={resolved} />;
  }

  return (
    <GluestackUIProvider mode={resolved}>
      <QueryClientProvider client={queryClient}>
        <AppConfigContext.Provider value={ctx}>
          {hydrated.token ? <PushSetup /> : null}
          <RouteGuard>
            <StatusBar style="auto" />
            <DrawerProvider>
              <ResponsiveShell>
                <Stack
                  screenOptions={{
                    animation: 'slide_from_right',
                    animationDuration: 240,
                    contentStyle: { backgroundColor: 'transparent' },
                    // Desktop: sidebar rail is always visible; no hamburger needed.
                    // Mobile: hamburger lives in the Stack/Tabs header via DrawerProvider.
                    headerShown: false,
                  }}
                >
                  {/* (tabs) has its own Tabs header with a hamburger injected
                      by (tabs)/_layout.tsx — keep headerShown:false here. */}
                  <Stack.Screen name="(tabs)" options={{ headerShown: false }} />

                  {/* run/[id]: show the Stack header with a hamburger on mobile.
                      The custom in-screen info bar still renders below it. */}
                  <Stack.Screen
                    name="run/[id]"
                    options={{
                      headerShown: true,
                      headerLeft: () => <HamburgerButton />,
                      headerRight: () => null,
                      title: 'Chat',
                    }}
                  />

                  <Stack.Screen
                    name="new"
                    options={{
                      headerShown: true,
                      title: 'New Chat',
                      presentation: 'modal',
                      headerBackTitle: 'Cancel',
                    }}
                  />
                  <Stack.Screen
                    name="sprites"
                    options={{
                      headerShown: true,
                      headerLeft: () => <HamburgerButton />,
                      headerRight: () => null,
                      title: 'Sprites',
                    }}
                  />
                  <Stack.Screen name="login" options={{ headerShown: false }} />
                </Stack>
              </ResponsiveShell>
            </DrawerProvider>
          </RouteGuard>
        </AppConfigContext.Provider>
      </QueryClientProvider>
    </GluestackUIProvider>
  );
}
