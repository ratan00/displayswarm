package com.displayswarm.client

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class UpdateCheckerTest {
    @Test fun parsesTagsWithAndWithoutPrefix() {
        assertEquals(listOf(0, 1, 0), UpdateChecker.parseVersion("v0.1.0"))
        assertEquals(listOf(1, 2), UpdateChecker.parseVersion("1.2-beta"))
        assertNull(UpdateChecker.parseVersion("nightly"))
    }

    @Test fun newerOnlyWhenHigher() {
        assertTrue(UpdateChecker.isNewer("0.1.0", "v0.1.1"))
        assertTrue(UpdateChecker.isNewer("0.9.0", "v0.10.0"))
        assertTrue(UpdateChecker.isNewer("0.1", "0.1.1"))
        assertFalse(UpdateChecker.isNewer("0.1.0", "v0.1.0"))
        assertFalse(UpdateChecker.isNewer("0.2.0", "v0.1.9"))
        assertFalse(UpdateChecker.isNewer("0.1.0", "garbage"))
    }
}
