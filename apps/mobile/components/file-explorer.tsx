import React, { useState, useEffect } from 'react';
import { FlatList, Pressable } from 'react-native';
import { useQuery } from '@tanstack/react-query';
import { listFsDir } from '../src/api';
import type { FsEntry } from '../src/api';
import { useAppConfig } from '../src/config-store';
import { Text } from '@/components/ui/text';
import { HStack } from '@/components/ui/hstack';
import { VStack } from '@/components/ui/vstack';
import { Button, ButtonText } from '@/components/ui/button';
import { Box } from '@/components/ui/box';
import {
  AlertDialog,
  AlertDialogBackdrop,
  AlertDialogContent,
  AlertDialogHeader,
  AlertDialogBody,
  AlertDialogFooter,
} from '@/components/ui/alert-dialog';

export interface FileExplorerProps {
  visible: boolean;
  selected: string[];
  onConfirm: (paths: string[]) => void;
  onCancel: () => void;
}

export function FileExplorer({ visible, selected, onConfirm, onCancel }: FileExplorerProps) {
  const { baseUrl, token } = useAppConfig();
  const [navStack, setNavStack] = useState<string[]>([]);
  const [draft, setDraft] = useState<string[]>([]);

  useEffect(() => {
    if (visible) {
      setDraft(selected);
      setNavStack([]);
    }
  }, [visible, selected]);

  const currentPath = navStack.length > 0 ? navStack[navStack.length - 1] : undefined;

  const { data, isLoading, isError } = useQuery({
    queryKey: ['fs-list', currentPath ?? ''],
    queryFn: () => listFsDir(baseUrl, token, currentPath),
    enabled: visible,
  });

  function drillIn(entry: FsEntry) {
    setNavStack((s) => [...s, entry.path]);
  }

  function goUp() {
    setNavStack((s) => s.slice(0, -1));
  }

  function toggleDir(entry: FsEntry) {
    setDraft((prev) =>
      prev.includes(entry.path) ? prev.filter((p) => p !== entry.path) : [...prev, entry.path],
    );
  }

  const breadcrumb = data
    ? [data.root, ...navStack.map((p) => p.split('/').pop() ?? p)].join(' / ')
    : '...';

  return (
    <AlertDialog isOpen={visible} onClose={onCancel} size="full">
      <AlertDialogBackdrop />
      <AlertDialogContent className="max-w-[760px] h-[86vh] p-0 overflow-hidden">
        <AlertDialogHeader className="px-4 pt-4 pb-2 border-b border-border">
          <VStack space="xs" className="flex-1">
            <HStack className="items-center justify-between">
              <Text size="lg" bold className="text-foreground">
                Browse directories
              </Text>
              {navStack.length > 0 ? (
                <Pressable onPress={goUp} className="px-3 py-1 rounded-md bg-muted">
                  <Text size="sm" className="text-foreground">
                    Back
                  </Text>
                </Pressable>
              ) : null}
            </HStack>
            <Text size="xs" className="text-muted-foreground" numberOfLines={2}>
              {breadcrumb}
            </Text>
          </VStack>
        </AlertDialogHeader>

        <AlertDialogBody className="flex-1 p-0">
          {draft.length > 0 ? (
            <Box className="px-4 py-2 border-b border-border">
              <HStack className="flex-wrap gap-1">
                {draft.map((p) => (
                  <Pressable
                    key={p}
                    onPress={() => setDraft((prev) => prev.filter((x) => x !== p))}
                    className="px-2 py-1 rounded-full bg-primary"
                  >
                    <Text size="xs" className="text-primary-foreground">
                      {p.split('/').pop() ?? p} ×
                    </Text>
                  </Pressable>
                ))}
              </HStack>
            </Box>
          ) : null}

          <Box className="flex-1">
            {isLoading ? (
              <Box className="flex-1 items-center justify-center">
                <Text className="text-muted-foreground">Loading...</Text>
              </Box>
            ) : null}
            {isError ? (
              <Box className="flex-1 items-center justify-center px-4">
                <Text className="text-destructive text-center">Failed to load directory.</Text>
              </Box>
            ) : null}
            {data ? (
              <FlatList
                data={data.entries}
                keyExtractor={(item) => item.path}
                renderItem={({ item }) => {
                  if (!item.isDir) {
                    return (
                      <HStack className="px-4 py-3 items-center border-b border-border/40">
                        <Text size="sm" className="text-muted-foreground flex-1" numberOfLines={1}>
                          {item.name}
                        </Text>
                      </HStack>
                    );
                  }
                  const isSelected = draft.includes(item.path);
                  return (
                    <Pressable onPress={() => toggleDir(item)}>
                      <HStack className="px-4 py-3 items-center border-b border-border/40">
                        <Box
                          className={
                            isSelected
                              ? 'w-5 h-5 rounded border border-primary bg-primary mr-3 items-center justify-center'
                              : 'w-5 h-5 rounded border border-border bg-background mr-3'
                          }
                        >
                          {isSelected ? (
                            <Text size="xs" className="text-primary-foreground leading-none">
                              ✓
                            </Text>
                          ) : null}
                        </Box>
                        <Text size="sm" className="text-foreground flex-1" numberOfLines={1}>
                          {item.name}
                        </Text>
                        <Pressable
                          onPress={(e) => {
                            e.stopPropagation();
                            drillIn(item);
                          }}
                          className="p-1 ml-2"
                          hitSlop={8}
                        >
                          <Text size="sm" className="text-muted-foreground">
                            ›
                          </Text>
                        </Pressable>
                      </HStack>
                    </Pressable>
                  );
                }}
              />
            ) : null}
          </Box>
        </AlertDialogBody>

        <AlertDialogFooter className="px-4 py-4 border-t border-border gap-3">
          <Button variant="outline" className="flex-1" onPress={onCancel}>
            <ButtonText>Cancel</ButtonText>
          </Button>
          <Button className="flex-1" onPress={() => onConfirm(draft)}>
            <ButtonText>Done{draft.length > 0 ? ` (${draft.length})` : ''}</ButtonText>
          </Button>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
