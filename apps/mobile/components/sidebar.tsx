/**
 * Sidebar — unified nav for desktop (persistent collapsible rail) and
 * mobile (drawer that slides in from the left over a dimmed backdrop).
 *
 * Prop API:
 *   mode        'desktop' | 'mobile-drawer'
 *   open        mobile-drawer only — controls visibility
 *   onClose     mobile-drawer only — called when backdrop or item tapped
 *
 * Desktop collapsed state is persisted to localStorage under
 * `howlerforge_sidebar_collapsed` (web only; native always expanded).
 *
 * Animation timings:
 *   drawer slide  240ms ease-out
 *   backdrop fade 180ms ease-out
 *   reduce-motion → instant (0ms)
 */
import { useEffect, useState, useMemo, useCallback, useRef } from 'react';
import { Platform, useColorScheme } from 'react-native';
import Animated, {
  Easing,
  useAnimatedStyle,
  useSharedValue,
  withTiming,
} from 'react-native-reanimated';
import { Feather } from '@expo/vector-icons';
import { useRouter, useSegments, useLocalSearchParams } from 'expo-router';

import { Box } from '@/components/ui/box';
import { Text } from '@/components/ui/text';
import { Pressable } from '@/components/ui/pressable';
import { ScrollView } from '@/components/ui/scroll-view';
import { VStack } from '@/components/ui/vstack';
import { HStack } from '@/components/ui/hstack';
import { Heading } from '@/components/ui/heading';
import { Button, ButtonText } from '@/components/ui/button';
import { Input, InputField } from '@/components/ui/input';
import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogBody,
  AlertDialogFooter,
} from '@/components/ui/alert-dialog';

import { StaggerEnter, usePulse, useReduceMotion } from '../src/animations';
import { WolfMark } from './wolf-mark';
import { relativeTime, truncate, statusColor } from '../src/utils';
import { useDebounced } from '../src/hooks/useDebounced';
import type { RunRow } from '../src/api';
import { useDeleteRun, useRuns } from '../src/hooks/useRuns';
import type { RunStatus } from './runs-filter-bar';

// ---------------------------------------------------------------------------
// Filter helpers
// ---------------------------------------------------------------------------

function parseStatusParam(raw: string | string[] | undefined): ReadonlySet<RunStatus> {
  if (!raw || (Array.isArray(raw) && raw.length === 0)) return new Set();
  const str = Array.isArray(raw) ? (raw[0] ?? '') : raw;
  if (!str) return new Set();
  return new Set(
    str
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean) as RunStatus[],
  );
}

function serializeStatuses(statuses: ReadonlySet<RunStatus>): string | undefined {
  return statuses.size === 0 ? undefined : Array.from(statuses).join(',');
}

function applyRunFilter(runs: RunRow[], query: string, statuses: ReadonlySet<RunStatus>): RunRow[] {
  const q = query.trim().toLowerCase();
  return runs.filter((run) => {
    if (statuses.size > 0 && !statuses.has(run.status as RunStatus)) return false;
    if (!q) return true;
    return (run.title ?? '').toLowerCase().includes(q) || run.prompt.toLowerCase().includes(q);
  });
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const STORAGE_KEY = 'howlerforge_sidebar_collapsed';
const FAVORITES_KEY = 'howlerforge_favorite_chats';
const DRAWER_WIDTH = 320;
const RAIL_EXPANDED = 304;
const RAIL_COLLAPSED = 56;

// ---------------------------------------------------------------------------
// localStorage helpers (web only; no-ops on native)
// ---------------------------------------------------------------------------

function readCollapsedSync(): boolean {
  if (Platform.OS !== 'web') return false;
  if (typeof window === 'undefined' || !window.localStorage) return false;
  try {
    return window.localStorage.getItem(STORAGE_KEY) === 'true';
  } catch {
    return false;
  }
}

function persistCollapsed(value: boolean): void {
  if (Platform.OS !== 'web') return;
  if (typeof window === 'undefined' || !window.localStorage) return;
  try {
    window.localStorage.setItem(STORAGE_KEY, String(value));
  } catch {
    // ignore
  }
}

function readFavoritesSync(): ReadonlySet<string> {
  if (Platform.OS !== 'web' || typeof window === 'undefined' || !window.localStorage)
    return new Set();
  try {
    const raw = window.localStorage.getItem(FAVORITES_KEY);
    return new Set(raw ? (JSON.parse(raw) as string[]) : []);
  } catch {
    return new Set();
  }
}

function persistFavorites(ids: ReadonlySet<string>): void {
  if (Platform.OS !== 'web' || typeof window === 'undefined' || !window.localStorage) return;
  try {
    window.localStorage.setItem(FAVORITES_KEY, JSON.stringify(Array.from(ids)));
  } catch {
    // ignore
  }
}

// ---------------------------------------------------------------------------
// RailRunItem — shared between desktop rail and mobile drawer
// ---------------------------------------------------------------------------

export function RailRunItem({
  run,
  isActive,
  onPress,
  onToggleFavorite,
  onDelete,
  favorite = false,
  index,
  collapsed = false,
}: {
  run: RunRow;
  isActive: boolean;
  onPress: () => void;
  onToggleFavorite?: () => void;
  onDelete?: () => void;
  favorite?: boolean;
  index: number;
  collapsed?: boolean;
}) {
  const color = statusColor(run.status);
  const pulseStyle = usePulse(run.status === 'running');
  const rawTitle = (run.title ?? '').trim();
  const rawPrompt = run.prompt.trim();
  const titleIsPrompt = rawTitle.length > 0 && rawTitle.toLowerCase() === rawPrompt.toLowerCase();
  const displayTitle = rawTitle && !titleIsPrompt ? rawTitle : truncate(rawPrompt, 46);

  if (collapsed) {
    // Mini-rail: show only a colored status dot
    return (
      <StaggerEnter delay={Math.min(index, 8) * 35}>
        <Pressable
          onPress={onPress}
          className={
            isActive
              ? 'w-14 h-10 items-center justify-center bg-sidebar-accent'
              : 'w-14 h-10 items-center justify-center data-[hover=true]:bg-sidebar-accent data-[active=true]:bg-sidebar-accent/80'
          }
          accessibilityLabel={run.title ?? truncate(run.prompt, 80)}
        >
          <Animated.View
            style={pulseStyle}
            className={`w-2.5 h-2.5 rounded-full ${color.bgClass}`}
          />
        </Pressable>
      </StaggerEnter>
    );
  }

  return (
    <StaggerEnter delay={Math.min(index, 8) * 35}>
      <Pressable
        onPress={onPress}
        className={
          isActive
            ? 'group mx-2 mb-1 h-16 px-3 rounded-xl border-l-2 border-l-primary bg-sidebar-accent justify-center'
            : 'group mx-2 mb-1 h-16 px-3 rounded-xl border-l-2 border-l-transparent justify-center data-[hover=true]:bg-sidebar-accent data-[active=true]:bg-sidebar-accent/80'
        }
      >
        <VStack space="xs">
          <Text
            size="sm"
            bold={isActive}
            className="text-foreground leading-tight"
            numberOfLines={1}
          >
            {displayTitle}
          </Text>

          <HStack space="xs" className="items-center">
            <Animated.View
              style={pulseStyle}
              className={`w-1.5 h-1.5 rounded-full ${color.dotClass}`}
            />
            <Text
              size="2xs"
              className="text-muted-foreground lowercase leading-none"
              style={{ fontSize: 11, lineHeight: 13 }}
              numberOfLines={1}
            >
              {color.label} • {relativeTime(run.updatedAt).replace(' ago', '')}
            </Text>
            <HStack className="ml-auto opacity-60" space="xs">
              <Pressable
                onPress={onToggleFavorite}
                accessibilityLabel={favorite ? 'Unfavorite chat' : 'Favorite chat'}
                className="w-5 h-5 items-center justify-center rounded-full data-[hover=true]:bg-background"
              >
                <Feather name="star" size={12} color={favorite ? '#f5a800' : '#7A746B'} />
              </Pressable>
              <Pressable
                onPress={onDelete}
                accessibilityLabel="Delete chat"
                className="w-5 h-5 items-center justify-center rounded-full data-[hover=true]:bg-destructive/10"
              >
                <Feather name="trash-2" size={12} color="#E45454" />
              </Pressable>
            </HStack>
          </HStack>
        </VStack>
      </Pressable>
    </StaggerEnter>
  );
}

// ---------------------------------------------------------------------------
// SidebarHeader
// ---------------------------------------------------------------------------

function SidebarHeader({
  collapsed,
  onToggleCollapse,
  showCollapseToggle,
}: {
  collapsed: boolean;
  onToggleCollapse?: (() => void) | undefined;
  showCollapseToggle: boolean;
}) {
  const colorScheme = useColorScheme();
  const iconColor = colorScheme === 'dark' ? '#fafafa' : '#171717';

  if (collapsed) {
    return (
      <Box className="w-14 h-16 items-center justify-center border-b border-sidebar-border">
        <Pressable
          onPress={onToggleCollapse}
          accessibilityLabel="Expand sidebar"
          className="items-center justify-center w-10 h-10 rounded-md data-[hover=true]:bg-sidebar-accent"
        >
          <WolfMark size={24} />
        </Pressable>
      </Box>
    );
  }

  return (
    <Box className="h-16 px-4 justify-center border-b border-sidebar-border">
      <HStack space="sm" className="items-center">
        <WolfMark size={24} />
        <Heading size="md" bold className="text-sidebar-foreground flex-1">
          HowlerForge
        </Heading>
        {showCollapseToggle ? (
          <Pressable
            onPress={onToggleCollapse}
            accessibilityLabel="Collapse sidebar"
            className="w-8 h-8 items-center justify-center rounded-md data-[hover=true]:bg-sidebar-accent"
          >
            <Feather name="chevron-left" size={18} color={iconColor} />
          </Pressable>
        ) : null}
      </HStack>
    </Box>
  );
}

// ---------------------------------------------------------------------------
// SidebarContent — the full inner column shared by both modes
// ---------------------------------------------------------------------------

function SidebarContent({
  collapsed = false,
  onNavPress,
  showCollapseToggle = false,
  onToggleCollapse,
}: {
  collapsed?: boolean;
  onNavPress?: (() => void) | undefined;
  showCollapseToggle?: boolean;
  onToggleCollapse?: (() => void) | undefined;
}) {
  const router = useRouter();
  const segments = useSegments();
  const { data } = useRuns();
  const deleteRun = useDeleteRun();
  const allRuns: RunRow[] = data?.runs ?? [];
  const colorScheme = useColorScheme();
  const iconColor = colorScheme === 'dark' ? '#fafafa' : '#171717';

  const segs = segments as readonly string[];
  const inSettings = segs.includes('settings');
  const inSprites = segs.includes('sprites');
  const runIdx = segs.indexOf('run');
  const activeRunId = runIdx >= 0 && runIdx + 1 < segs.length ? segs[runIdx + 1] : null;
  const inRuns = !inSettings && !inSprites && !activeRunId;

  // Filter state — seeded from URL params so back-nav restores them
  const params = useLocalSearchParams<{ q?: string; status?: string }>();
  const [query, setQuery] = useState<string>(() =>
    Array.isArray(params.q) ? (params.q[0] ?? '') : (params.q ?? ''),
  );
  const [selectedStatuses, setSelectedStatuses] = useState<ReadonlySet<RunStatus>>(() =>
    parseStatusParam(params.status),
  );
  const [favoriteIds, setFavoriteIds] = useState<ReadonlySet<string>>(() => readFavoritesSync());
  const [pendingDeleteRun, setPendingDeleteRun] = useState<RunRow | null>(null);

  const debouncedQuery = useDebounced(query, 200);

  // Sync URL whenever debounced query or status selection changes. Skip the
  // first run — state was just hydrated from URL params and Root Layout may
  // not be mounted yet (would throw "Attempted to navigate before mounting").
  const didMountRef = useRef(false);
  useEffect(() => {
    if (!didMountRef.current) {
      didMountRef.current = true;
      return;
    }
    router.setParams({
      q: debouncedQuery || undefined,
      status: serializeStatuses(selectedStatuses),
    } as Record<string, string | undefined>);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedQuery, selectedStatuses]);

  const handleStatusChange = useCallback((next: ReadonlySet<RunStatus>) => {
    setSelectedStatuses(next);
  }, []);

  const filteredRuns = useMemo(() => {
    const filtered = applyRunFilter(allRuns, debouncedQuery, selectedStatuses);
    return [...filtered].sort((a, b) => {
      const af = favoriteIds.has(a.id) ? 1 : 0;
      const bf = favoriteIds.has(b.id) ? 1 : 0;
      if (af !== bf) return bf - af;
      return new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime();
    });
  }, [allRuns, debouncedQuery, favoriteIds, selectedStatuses]);

  const isFiltered = debouncedQuery.trim().length > 0 || selectedStatuses.size > 0;

  function handleClearFilters() {
    setQuery('');
    setSelectedStatuses(new Set());
    router.setParams({ q: undefined, status: undefined } as Record<string, string | undefined>);
  }

  function nav(route: string) {
    onNavPress?.();
    router.push(route as never);
  }

  function toggleFavorite(id: string) {
    setFavoriteIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      persistFavorites(next);
      return next;
    });
  }

  function performDelete(run: RunRow) {
    void deleteRun.mutateAsync(run.id).then(() => {
      setPendingDeleteRun(null);
      setFavoriteIds((prev) => {
        const next = new Set(prev);
        next.delete(run.id);
        persistFavorites(next);
        return next;
      });
      if (activeRunId === run.id) router.replace('/(tabs)' as never);
    });
  }

  if (collapsed) {
    // Mini-rail: icons only
    return (
      <VStack
        className="h-full bg-sidebar border-r border-sidebar-border"
        style={{ width: RAIL_COLLAPSED }}
      >
        <SidebarHeader
          collapsed
          onToggleCollapse={onToggleCollapse}
          showCollapseToggle={showCollapseToggle}
        />

        {/* New chat icon */}
        <Box className="items-center py-3 border-b border-sidebar-border">
          <Pressable
            onPress={() => nav('/new')}
            accessibilityLabel="New chat"
            className="w-10 h-10 items-center justify-center rounded-full bg-primary data-[hover=true]:bg-primary/90"
          >
            <Feather name="plus" size={18} color="#171717" />
          </Pressable>
        </Box>

        {/* Chat list (dots) — collapsed rail shows unfiltered full list */}
        <Box className="flex-1">
          <ScrollView className="flex-1">
            {allRuns.map((run, idx) => (
              <RailRunItem
                key={run.id}
                run={run}
                index={idx}
                isActive={activeRunId === run.id}
                onPress={() => nav(`/run/${run.id}`)}
                favorite={favoriteIds.has(run.id)}
                collapsed
              />
            ))}
          </ScrollView>
        </Box>

        {/* Bottom nav icons */}
        <Box className="border-t border-sidebar-border">
          <Pressable
            onPress={() => nav('/(tabs)')}
            accessibilityLabel="Chats"
            className={
              inRuns
                ? 'w-14 h-12 items-center justify-center bg-sidebar-accent'
                : 'w-14 h-12 items-center justify-center data-[hover=true]:bg-sidebar-accent'
            }
          >
            <Feather name="list" size={18} color={iconColor} />
          </Pressable>
          <Pressable
            onPress={() => nav('/sprites')}
            accessibilityLabel="Sprites"
            className={
              inSprites
                ? 'w-14 h-12 items-center justify-center bg-sidebar-accent'
                : 'w-14 h-12 items-center justify-center data-[hover=true]:bg-sidebar-accent'
            }
          >
            <Feather name="cpu" size={18} color={iconColor} />
          </Pressable>
          <Pressable
            onPress={() => nav('/(tabs)/settings')}
            accessibilityLabel="Settings"
            className={
              inSettings
                ? 'w-14 h-12 items-center justify-center bg-sidebar-accent'
                : 'w-14 h-12 items-center justify-center data-[hover=true]:bg-sidebar-accent'
            }
          >
            <Feather name="settings" size={18} color={iconColor} />
          </Pressable>
        </Box>
      </VStack>
    );
  }

  return (
    <>
      <VStack
        className="h-full bg-sidebar border-r border-sidebar-border"
        style={{ width: collapsed ? RAIL_COLLAPSED : undefined }}
      >
        <SidebarHeader
          collapsed={false}
          onToggleCollapse={onToggleCollapse}
          showCollapseToggle={showCollapseToggle}
        />

        {/* New chat button */}
        <Box className="px-3 pb-3">
          <Button variant="default" size="sm" onPress={() => nav('/new')} className="w-full">
            <ButtonText>+ New chat</ButtonText>
          </Button>
        </Box>

        {/* Sidebar search — navigation-only, no dashboard filters here. */}
        <Box className="px-3 pb-3">
          <Input className="h-10 bg-background border-border">
            <InputField
              value={query}
              onChangeText={setQuery}
              placeholder="Search chats…"
              clearButtonMode="while-editing"
              autoCapitalize="none"
              autoCorrect={false}
              className="text-sm"
            />
          </Input>
        </Box>

        {/* Chat list */}
        <Box className="flex-1">
          <ScrollView className="flex-1">
            {filteredRuns.length === 0 ? (
              <VStack className="p-4" space="xs">
                <Text size="xs" className="text-muted-foreground">
                  {isFiltered
                    ? `No chats match${debouncedQuery.trim() ? ` "${debouncedQuery.trim()}"` : ' the selected filters'}.`
                    : 'No chats yet.'}
                </Text>
                {isFiltered && (
                  <Pressable onPress={handleClearFilters}>
                    <Text size="xs" className="text-primary underline">
                      Clear filters
                    </Text>
                  </Pressable>
                )}
              </VStack>
            ) : (
              filteredRuns.map((run, idx) => (
                <RailRunItem
                  key={run.id}
                  run={run}
                  index={idx}
                  isActive={activeRunId === run.id}
                  onPress={() => nav(`/run/${run.id}`)}
                  favorite={favoriteIds.has(run.id)}
                  onToggleFavorite={() => toggleFavorite(run.id)}
                  onDelete={() => setPendingDeleteRun(run)}
                />
              ))
            )}
          </ScrollView>
        </Box>

        {/* Bottom nav */}
        <Box className="border-t border-sidebar-border">
          <Pressable
            onPress={() => nav('/(tabs)')}
            className={
              inRuns
                ? 'px-4 py-3 bg-sidebar-accent'
                : 'px-4 py-3 data-[hover=true]:bg-sidebar-accent'
            }
          >
            <HStack space="sm" className="items-center">
              <Feather name="list" size={16} color={iconColor} />
              <Text size="sm" className="text-sidebar-foreground">
                Chats
              </Text>
            </HStack>
          </Pressable>
          <Pressable
            onPress={() => nav('/sprites')}
            className={
              inSprites
                ? 'px-4 py-3 bg-sidebar-accent'
                : 'px-4 py-3 data-[hover=true]:bg-sidebar-accent'
            }
          >
            <HStack space="sm" className="items-center">
              <Feather name="cpu" size={16} color={iconColor} />
              <Text size="sm" className="text-sidebar-foreground">
                Sprites
              </Text>
            </HStack>
          </Pressable>
          <Pressable
            onPress={() => nav('/(tabs)/settings')}
            className={
              inSettings
                ? 'px-4 py-3 bg-sidebar-accent'
                : 'px-4 py-3 data-[hover=true]:bg-sidebar-accent'
            }
          >
            <HStack space="sm" className="items-center">
              <Feather name="settings" size={16} color={iconColor} />
              <Text size="sm" className="text-sidebar-foreground">
                Settings
              </Text>
            </HStack>
          </Pressable>
        </Box>
      </VStack>
      <AlertDialog isOpen={!!pendingDeleteRun} onClose={() => setPendingDeleteRun(null)}>
        <AlertDialogBackdrop />
        <AlertDialogContent>
          <AlertDialogHeader>
            <Heading size="md" className="text-foreground">
              Delete chat?
            </Heading>
          </AlertDialogHeader>
          <AlertDialogBody>
            <Text size="sm" className="text-muted-foreground">
              This removes the chat and its timeline. This cannot be undone.
            </Text>
          </AlertDialogBody>
          <AlertDialogFooter>
            <Button variant="outline" onPress={() => setPendingDeleteRun(null)}>
              <ButtonText>Cancel</ButtonText>
            </Button>
            <Button
              variant="destructive"
              isDisabled={deleteRun.isPending}
              onPress={() => pendingDeleteRun && performDelete(pendingDeleteRun)}
            >
              <ButtonText>{deleteRun.isPending ? 'Deleting…' : 'Delete'}</ButtonText>
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

// ---------------------------------------------------------------------------
// Sidebar — public component
// ---------------------------------------------------------------------------

export type SidebarMode = 'desktop' | 'mobile-drawer';

export interface SidebarProps {
  mode: SidebarMode;
  /** mobile-drawer only: controls open/closed state */
  open?: boolean;
  /** mobile-drawer only: called when the user dismisses the drawer */
  onClose?: (() => void) | undefined;
}

export function Sidebar({ mode, open = false, onClose }: SidebarProps) {
  const reduceMotion = useReduceMotion();

  // ------- desktop mode -------
  const [collapsed, setCollapsed] = useState<boolean>(() => readCollapsedSync());

  function toggleCollapsed() {
    const next = !collapsed;
    setCollapsed(next);
    persistCollapsed(next);
  }

  if (mode === 'desktop') {
    const railWidth = collapsed ? RAIL_COLLAPSED : RAIL_EXPANDED;
    return (
      <Box style={{ width: railWidth }}>
        <SidebarContent
          collapsed={collapsed}
          showCollapseToggle
          onToggleCollapse={toggleCollapsed}
        />
      </Box>
    );
  }

  // ------- mobile-drawer mode -------
  return <MobileDrawer open={open} onClose={onClose} reduceMotion={reduceMotion} />;
}

// ---------------------------------------------------------------------------
// MobileDrawer — animated drawer + backdrop (mobile only)
// ---------------------------------------------------------------------------

function MobileDrawer({
  open,
  onClose,
  reduceMotion,
}: {
  open: boolean;
  onClose?: (() => void) | undefined;
  reduceMotion: boolean;
}) {
  // translateX: 12 = visible/floating, -(DRAWER_WIDTH + 24) = hidden off-screen left
  const translateX = useSharedValue(-(DRAWER_WIDTH + 24));
  const backdropOpacity = useSharedValue(0);

  useEffect(() => {
    if (open) {
      if (reduceMotion) {
        translateX.value = 12;
        backdropOpacity.value = 0.5;
      } else {
        translateX.value = withTiming(12, {
          duration: 240,
          easing: Easing.out(Easing.cubic),
        });
        backdropOpacity.value = withTiming(0.5, {
          duration: 180,
          easing: Easing.out(Easing.cubic),
        });
      }
    } else {
      if (reduceMotion) {
        translateX.value = -(DRAWER_WIDTH + 24);
        backdropOpacity.value = 0;
      } else {
        translateX.value = withTiming(-(DRAWER_WIDTH + 24), {
          duration: 240,
          easing: Easing.out(Easing.cubic),
        });
        backdropOpacity.value = withTiming(0, {
          duration: 180,
          easing: Easing.out(Easing.cubic),
        });
      }
    }
  }, [open, reduceMotion, translateX, backdropOpacity]);

  // Merge static layout into the animated style so we never pass
  // style={[animatedStyle, {…}]} — that combo trips RN-Web's CSSStyleDeclaration
  // indexed-property setter on commit. On web, also avoid the
  // `transform: [{ translateX }]` array — RN-Web 0.20 mis-applies it as an
  // indexed CSS property. Use `left` instead so the drawer slides in by
  // animating its X offset.
  const drawerStyle = useAnimatedStyle(() => {
    if (Platform.OS === 'web') {
      return {
        position: 'absolute',
        top: 12,
        left: translateX.value,
        bottom: 12,
        width: DRAWER_WIDTH,
        borderRadius: 28,
        overflow: 'hidden',
        boxShadow: '0 20px 60px rgba(0,0,0,0.28)',
      };
    }
    return {
      transform: [{ translateX: translateX.value }],
      position: 'absolute',
      top: 12,
      left: 12,
      bottom: 12,
      width: DRAWER_WIDTH,
      borderRadius: 28,
      overflow: 'hidden',
    };
  });

  const backdropStyle = useAnimatedStyle(() => ({
    opacity: backdropOpacity.value,
    position: 'absolute',
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    backgroundColor: '#000',
  }));

  // Use absolute positioning everywhere. Previously web used `position: 'fixed'`
  // via `as never`, which RN-Web sends through a code path that trips
  // "Failed to set an indexed property [0] on 'CSSStyleDeclaration'" on
  // commit. `absolute` is universally supported and fills the parent View.
  // `pointerEvents` is set as a prop on Animated.View, not inside the style.
  const containerStyle = {
    position: 'absolute' as const,
    top: 0,
    left: 0,
    right: 0,
    bottom: 0,
    zIndex: 50,
  };

  return (
    <Animated.View style={containerStyle} pointerEvents={open ? 'auto' : 'none'}>
      {/* Dimmed backdrop */}
      <Animated.View style={backdropStyle} pointerEvents={open ? 'auto' : 'none'}>
        <Pressable onPress={onClose} style={{ flex: 1 }} accessibilityLabel="Close navigation" />
      </Animated.View>

      {/* Drawer panel */}
      <Animated.View style={drawerStyle}>
        <SidebarContent onNavPress={onClose} showCollapseToggle={false} />
      </Animated.View>
    </Animated.View>
  );
}
