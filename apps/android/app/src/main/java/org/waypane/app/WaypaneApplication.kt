package org.waypane.app

import android.app.Application
import androidx.lifecycle.ViewModelProvider
import androidx.lifecycle.ViewModelStore
import androidx.lifecycle.ViewModelStoreOwner

class WaypaneApplication : Application(), ViewModelStoreOwner {
    override val viewModelStore = ViewModelStore()

    val sessionModel: WaypaneViewModel by lazy {
        ViewModelProvider(
            this,
            ViewModelProvider.AndroidViewModelFactory.getInstance(this),
        )[WaypaneViewModel::class.java]
    }
}
