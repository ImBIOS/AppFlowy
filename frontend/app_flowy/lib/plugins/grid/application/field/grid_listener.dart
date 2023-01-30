import 'package:app_flowy/core/grid_notification.dart';
import 'package:dartz/dartz.dart';
import 'package:appflowy_backend/protobuf/flowy-error/errors.pb.dart';
import 'package:appflowy_backend/protobuf/flowy-database/notification.pb.dart';
import 'package:flowy_infra/notifier.dart';
import 'dart:async';
import 'dart:typed_data';
import 'package:appflowy_backend/protobuf/flowy-database/field_entities.pb.dart';

typedef UpdateFieldNotifiedValue = Either<GridFieldChangesetPB, FlowyError>;

class GridFieldsListener {
  final String gridId;
  PublishNotifier<UpdateFieldNotifiedValue>? updateFieldsNotifier =
      PublishNotifier();
  DatabaseNotificationListener? _listener;
  GridFieldsListener({required this.gridId});

  void start(
      {required void Function(UpdateFieldNotifiedValue) onFieldsChanged}) {
    updateFieldsNotifier?.addPublishListener(onFieldsChanged);
    _listener = DatabaseNotificationListener(
      objectId: gridId,
      handler: _handler,
    );
  }

  void _handler(DatabaseNotification ty, Either<Uint8List, FlowyError> result) {
    switch (ty) {
      case DatabaseNotification.DidUpdateGridFields:
        result.fold(
          (payload) => updateFieldsNotifier?.value =
              left(GridFieldChangesetPB.fromBuffer(payload)),
          (error) => updateFieldsNotifier?.value = right(error),
        );
        break;
      default:
        break;
    }
  }

  Future<void> stop() async {
    await _listener?.stop();
    updateFieldsNotifier?.dispose();
    updateFieldsNotifier = null;
  }
}
