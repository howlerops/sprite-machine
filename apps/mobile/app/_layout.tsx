import { useState, useEffect, useCallback } from 'react';
import { Stack, useRouter, useSegments } from 'expo-router';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { StatusBar } from 'expo-status-bar';
import { AppConfigContext } from '../src/config-store';
import { getToken, getBaseUrl, DEFAULT_BASE_URL } from '../src/storage';
import { usePushRegistration } from '../src/push';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      staleTime: 5_000,
    },
  },
});

function RouteGuard({ children }: { children: React.ReactNode }) {
  const segments = useSegments();
  const router = useRouter();
  const [token, setTokenState] = useState<string | null>(null);
  const [baseUrl, setBaseUrlState] = useState<string>(DEFAULT_BASE_URL);
  const [ready, setReady] = useState(false);

  useEffect(() => {
    void (async () => {
      const [t, u] = await Promise.all([getToken(), getBaseUrl()]);
      setTokenState(t);
      setBaseUrlState(u);
      setReady(true);
    })();
  }, []);

  useEffect(() => {
    if (!ready) return;
    const inSettings = segments.includes('settings');
    if (!token && !inSettings) {
      router.replace('/(tabs)/settings');
    }
  }, [ready, token, segments, router]);

  const setToken = useCallback((t: string | null) => setTokenState(t), []);
  const setBaseUrl = useCallback((u: string) => setBaseUrlState(u), []);

  if (!ready) return null;

  return (
    <AppConfigContext.Provider value={{ token, baseUrl, setToken, setBaseUrl }}>
      <PushSetup />
      {children}
    </AppConfigContext.Provider>
  );
}

function PushSetup() {
  usePushRegistration();
  return null;
}

export default function RootLayout() {
  return (
    <QueryClientProvider client={queryClient}>
      <RouteGuard>
        <StatusBar style="auto" />
        <Stack>
          <Stack.Screen name="(tabs)" options={{ headerShown: false }} />
          <Stack.Screen name="run/[id]" options={{ title: 'Run', headerBackTitle: 'Runs' }} />
          <Stack.Screen
            name="new"
            options={{ title: 'New Run', presentation: 'modal', headerBackTitle: 'Cancel' }}
          />
        </Stack>
      </RouteGuard>
    </QueryClientProvider>
  );
}
