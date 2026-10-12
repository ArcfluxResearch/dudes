// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useEffectEvent, useRef } from 'react';

import {
  deliverDesktopNotification, desktopNotificationPreference, notificationArrivals, seedNotificationHistory,
} from './desktopNotifications.ts';
import type { NotificationDestination, NotificationHistory } from './desktopNotifications.ts';
import { subscribeToProjection } from './floor/projection.ts';
import { getFloorState } from './floor/store.ts';

export function useDesktopNotifications(open: (target: NotificationDestination) => void): void {
  const history = useRef<NotificationHistory | null>(null);
  const openCurrent = useEffectEvent(open);
  useEffect(() => {
    history.current = seedNotificationHistory(getFloorState());
    return subscribeToProjection((event) => {
      const step = notificationArrivals(history.current, event);
      history.current = step.history;
      for (const notice of step.notices) deliverDesktopNotification(notice, desktopNotificationPreference, openCurrent);
    });
  }, []);
}
